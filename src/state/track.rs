//! Car positions and speeds from the `.z` topics, stopped-car detection,
//! and building a track outline from position samples.

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, TimeDelta, Utc};

/// How much position and speed history is kept per car.
const KEEP: TimeDelta = TimeDelta::seconds(10);
/// A car is stationary when it has been under this speed (km/h)...
const STOP_SPEED: f64 = 5.0;
/// ...for this long...
const STOP_FOR: TimeDelta = TimeDelta::seconds(5);
/// ...and moved less than this in position units (decimetres).
const STOP_MOVE: f64 = 50.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CarPos {
    pub ts: DateTime<Utc>,
    pub x: f64,
    pub y: f64,
    pub on_track: bool,
}

#[derive(Debug, Default)]
pub struct Cars {
    pos: HashMap<String, VecDeque<CarPos>>,
    speed: HashMap<String, VecDeque<(DateTime<Utc>, f64)>>,
}

impl Cars {
    pub fn has_positions(&self) -> bool {
        !self.pos.is_empty()
    }

    pub fn positions(&self) -> impl Iterator<Item = (&String, &CarPos)> {
        self.pos.iter().filter_map(|(n, q)| Some((n, q.back()?)))
    }

    pub fn push_pos(&mut self, num: &str, p: CarPos) {
        let q = self.pos.entry(num.to_owned()).or_default();
        if q.back().is_some_and(|b| b.ts > p.ts) {
            return;
        }
        q.push_back(p);
        while q.front().is_some_and(|f| p.ts - f.ts > KEEP) {
            q.pop_front();
        }
    }

    pub fn push_speed(&mut self, num: &str, ts: DateTime<Utc>, kmh: f64) {
        let q = self.speed.entry(num.to_owned()).or_default();
        if q.back().is_some_and(|b| b.0 > ts) {
            return;
        }
        q.push_back((ts, kmh));
        while q.front().is_some_and(|f| ts - f.0 > KEEP) {
            q.pop_front();
        }
    }

    /// `Some(true)` when the car has been slower than 5 km/h for 5 s and
    /// hasn't moved; `None` without telemetry. Only time after `since`
    /// counts: at the start every car has been still on the grid.
    pub fn stationary(&self, num: &str, since: Option<DateTime<Utc>>) -> Option<bool> {
        let q = self.speed.get(num)?;
        let &(now, _) = q.back()?;
        let from = now - STOP_FOR;
        if q.front()?.0 > from || since.is_some_and(|s| s > from) {
            return Some(false); // not enough history yet
        }
        if q.iter().filter(|s| s.0 >= from).any(|s| s.1 >= STOP_SPEED) {
            return Some(false);
        }
        let Some(p) = self.pos.get(num) else {
            return Some(true);
        };
        let Some(last) = p.back() else {
            return Some(true);
        };
        let moved = p
            .iter()
            .filter(|s| s.ts >= from)
            .any(|s| (s.x - last.x).hypot(s.y - last.y) > STOP_MOVE);
        Some(!moved)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StopInfo {
    pub since: DateTime<Utc>,
    pub lap: u32,
}

#[derive(Debug, Default)]
pub struct StopTracker {
    pub stopped: HashMap<String, StopInfo>,
}

impl StopTracker {
    /// Returns whether anything changed.
    pub fn update(&mut self, num: &str, stopped: bool, now: DateTime<Utc>, lap: u32) -> bool {
        match (stopped, self.stopped.contains_key(num)) {
            (true, false) => {
                self.stopped
                    .insert(num.to_owned(), StopInfo { since: now, lap });
                true
            }
            (false, true) => {
                self.stopped.remove(num);
                true
            }
            _ => false,
        }
    }
}

/// Records one car's positions over a clean lap, as a fallback track
/// outline when no circuit geometry is available.
#[derive(Debug, Default)]
pub struct OutlineBuilder {
    car: Option<String>,
    collecting: bool,
    pts: Vec<(f64, f64)>,
    pub done: Option<Vec<(f64, f64)>>,
}

impl OutlineBuilder {
    pub fn on_lap(&mut self, num: &str, clean: bool) {
        if self.done.is_some() {
            return;
        }
        let car = self.car.get_or_insert_with(|| num.to_owned());
        if car != num {
            return;
        }
        if self.collecting && clean && self.pts.len() > 100 {
            self.done = Some(std::mem::take(&mut self.pts));
            tracing::info!("built track outline from car {num}");
        } else {
            self.collecting = true;
            self.pts.clear();
        }
    }

    pub fn on_position(&mut self, num: &str, x: f64, y: f64) {
        if !self.collecting || self.done.is_some() || self.car.as_deref() != Some(num) {
            return;
        }
        if self.pts.last() != Some(&(x, y)) {
            self.pts.push((x, y));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + s, 0).unwrap()
    }

    #[test]
    fn stationary_needs_five_slow_seconds() {
        let mut c = Cars::default();
        assert_eq!(c.stationary("1", None), None);
        for s in 0..4 {
            c.push_speed("1", t(s), 0.0);
        }
        assert_eq!(c.stationary("1", None), Some(false));
        for s in 4..8 {
            c.push_speed("1", t(s), 2.0);
        }
        assert_eq!(c.stationary("1", None), Some(true));
        // Lights out at 4 s: the car hasn't been still for 5 s since.
        assert_eq!(c.stationary("1", Some(t(4))), Some(false));
        assert_eq!(c.stationary("1", Some(t(2))), Some(true));
        c.push_speed("1", t(8), 40.0);
        assert_eq!(c.stationary("1", None), Some(false));
    }

    #[test]
    fn moving_position_is_not_stationary() {
        let mut c = Cars::default();
        for s in 0..8 {
            c.push_speed("1", t(s), 0.0);
            c.push_pos(
                "1",
                CarPos {
                    ts: t(s),
                    x: s as f64 * 100.0,
                    y: 0.0,
                    on_track: true,
                },
            );
        }
        assert_eq!(c.stationary("1", None), Some(false));
    }

    #[test]
    fn history_is_bounded() {
        let mut c = Cars::default();
        for s in 0..100 {
            c.push_speed("1", t(s), 100.0);
        }
        assert!(c.speed.get("1").unwrap().len() <= 11);
    }

    #[test]
    fn outline_needs_a_clean_lap() {
        let mut o = OutlineBuilder::default();
        o.on_lap("1", false);
        for i in 0..200 {
            o.on_position("1", i as f64, 0.0);
            o.on_position("4", 0.0, i as f64);
        }
        o.on_lap("4", true);
        assert!(o.done.is_none());
        o.on_lap("1", false); // pit lap: start over
        assert!(o.done.is_none());
        for i in 0..200 {
            o.on_position("1", i as f64, 1.0);
        }
        o.on_lap("1", true);
        assert_eq!(o.done.as_ref().unwrap().len(), 200);
    }
}
