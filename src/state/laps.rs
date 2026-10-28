//! Per-driver lap history and the catching / falling-behind trend.

use std::collections::{HashMap, VecDeque};

use serde_json::Value;

use super::parse::{Gap, as_bool, as_u32, parse_gap, parse_laptime};

/// Completed laps kept per driver. A few more than the trend window so
/// lap numbers still line up with the car ahead.
const HISTORY: usize = 8;
/// Trend below this many seconds per lap shows as `=`.
pub const TREND_DEADBAND: f64 = 0.05;
/// When the lap-time and interval figures disagree by more than this, the
/// interval figure wins: it includes traffic.
const DISAGREE: f64 = 0.3;
/// Laps-to-catch estimates beyond this are noise and not shown.
const MAX_LAPS_TO_CATCH: u32 = 15;

#[derive(Debug, Clone, PartialEq)]
pub struct LapRecord {
    pub lap: u32,
    pub time: Option<f64>,
    /// Interval to the car ahead when the lap was completed.
    pub interval: Option<f64>,
    pub pit: bool,
    /// Run at least partly under SC, VSC or red.
    pub neutral: bool,
}

impl LapRecord {
    fn valid(&self) -> bool {
        self.time.is_some() && !self.pit && !self.neutral && self.lap > 1
    }
}

#[derive(Debug, Default)]
struct DriverLaps {
    laps: u32,
    history: VecDeque<LapRecord>,
    /// The same laps, kept when the car ahead changes: for the driver's
    /// own pace.
    own: VecDeque<LapRecord>,
    /// Car ahead when the history was recorded.
    ahead: Option<String>,
    cur_pit: bool,
    cur_neutral: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrendMode {
    Avg3,
    #[default]
    Last,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trend {
    /// Seconds per lap; positive means catching the car ahead.
    pub per_lap: f64,
    pub from_interval: bool,
    pub laps_to_catch: Option<u32>,
}

#[derive(Debug, Default)]
pub struct LapTracker {
    drivers: HashMap<String, DriverLaps>,
    neutral: bool,
}

impl LapTracker {
    /// Sets the lap baseline from a snapshot so laps run before we joined
    /// aren't treated as just completed.
    pub fn init_from(&mut self, timing: &Value, neutral: bool) {
        self.drivers.clear();
        self.neutral = neutral;
        if let Some(lines) = timing["Lines"].as_object() {
            for (num, line) in lines {
                self.drivers.insert(
                    num.clone(),
                    DriverLaps {
                        laps: as_u32(&line["NumberOfLaps"]).unwrap_or(0),
                        // The lap in progress started before we joined.
                        cur_neutral: true,
                        ..Default::default()
                    },
                );
            }
        }
    }

    /// Track status changed. Any lap in progress under SC/VSC/red is marked.
    pub fn set_neutral(&mut self, neutral: bool) {
        self.neutral = neutral;
        if neutral {
            for d in self.drivers.values_mut() {
                d.cur_neutral = true;
            }
        }
    }

    /// Processes one TimingData delta; `timing` is the merged topic after
    /// applying it. Returns the drivers that completed a lap.
    pub fn on_timing(&mut self, delta: &Value, timing: &Value) -> Vec<String> {
        let Some(lines) = delta.get("Lines").and_then(Value::as_object) else {
            return Vec::new();
        };
        let neutral = self.neutral;
        let mut done = Vec::new();
        for (num, ld) in lines {
            let Some(line) = timing["Lines"].get(num) else {
                continue;
            };
            let d = self.drivers.entry(num.clone()).or_default();
            let in_pit_lane =
                as_bool(&line["InPit"]) == Some(true) || as_bool(&line["PitOut"]) == Some(true);
            if in_pit_lane {
                d.cur_pit = true;
            }
            let delta_time = ld
                .get("LastLapTime")
                .and_then(|l| l.get("Value"))
                .and_then(Value::as_str)
                .map(parse_laptime);
            let Some(n) = ld.get("NumberOfLaps").and_then(as_u32) else {
                // The lap time can arrive after the lap count.
                if let Some(t) = delta_time {
                    for back in [d.history.back_mut(), d.own.back_mut()]
                        .into_iter()
                        .flatten()
                    {
                        if back.lap == d.laps && back.time.is_none() {
                            back.time = t;
                        }
                    }
                }
                continue;
            };
            if n < d.laps {
                // A new session or session part restarted the count.
                *d = DriverLaps::default();
            }
            if n <= d.laps {
                continue;
            }
            let ahead = car_ahead(timing, line);
            if d.ahead != ahead {
                d.history.clear();
                d.ahead = ahead;
            }
            let rec = LapRecord {
                lap: n,
                time: delta_time.flatten(),
                interval: parse_gap(
                    line["IntervalToPositionAhead"]["Value"]
                        .as_str()
                        .unwrap_or(""),
                )
                .seconds(),
                pit: d.cur_pit,
                neutral: d.cur_neutral,
            };
            for q in [&mut d.history, &mut d.own] {
                q.push_back(rec.clone());
                if q.len() > HISTORY {
                    q.pop_front();
                }
            }
            d.laps = n;
            d.cur_pit = in_pit_lane;
            d.cur_neutral = neutral;
            done.push(num.clone());
        }
        done
    }

    /// Whether the driver's last completed lap was clean.
    pub fn last_valid(&self, num: &str) -> bool {
        self.drivers
            .get(num)
            .and_then(|d| d.history.back())
            .is_some_and(LapRecord::valid)
    }

    /// The driver's own pace: seconds per lap faster (positive) or slower
    /// over the newest run of clean laps, so a pit stop, SC or VSC starts
    /// it again. Avg3 spans up to 3 laps, Last compares the last two.
    pub fn pace(&self, num: &str, mode: TrendMode) -> Option<f64> {
        let d = self.drivers.get(num)?;
        let window = match mode {
            TrendMode::Avg3 => 3,
            TrendMode::Last => 1,
        };
        let run: Vec<&LapRecord> = d
            .own
            .iter()
            .rev()
            .take_while(|r| r.valid())
            .take(window + 1)
            .collect();
        match (run.first(), run.last()) {
            (Some(new), Some(old)) if new.lap > old.lap => {
                Some((old.time? - new.time?) / f64::from(new.lap - old.lap))
            }
            _ => None,
        }
    }

    pub fn trend(
        &self,
        num: &str,
        ahead: Option<&str>,
        interval_now: Gap,
        mode: TrendMode,
    ) -> Option<Trend> {
        let ahead = ahead?;
        let d = self.drivers.get(num)?;
        if d.ahead.as_deref() != Some(ahead) {
            return None;
        }
        let a = self.drivers.get(ahead)?;
        let window = match mode {
            TrendMode::Avg3 => 3,
            TrendMode::Last => 1,
        };

        let deltas: Vec<f64> = d
            .history
            .iter()
            .rev()
            .filter(|r| r.valid())
            .filter_map(|r| {
                let ar = a.history.iter().find(|x| x.lap == r.lap && x.valid())?;
                Some(ar.time? - r.time?)
            })
            .take(window)
            .collect();
        let from_laps =
            (!deltas.is_empty()).then(|| deltas.iter().sum::<f64>() / deltas.len() as f64);

        // Change in interval over the newest run of laps clean for both
        // cars: the car ahead pitting shrinks the interval just as much.
        let run: Vec<&LapRecord> = d
            .history
            .iter()
            .rev()
            .take_while(|r| {
                r.valid()
                    && r.interval.is_some()
                    && a.history.iter().any(|x| x.lap == r.lap && x.valid())
            })
            .take(window + 1)
            .collect();
        let from_interval = match (run.first(), run.last()) {
            (Some(new), Some(old)) if new.lap > old.lap => {
                Some((old.interval? - new.interval?) / f64::from(new.lap - old.lap))
            }
            _ => None,
        };

        let (per_lap, from_interval) = match (from_laps, from_interval) {
            (Some(l), Some(i)) if (l - i).abs() > DISAGREE => (i, true),
            (Some(l), _) => (l, false),
            (None, Some(i)) => (i, true),
            (None, None) => return None,
        };
        let laps_to_catch = match interval_now {
            Gap::Seconds(gap) if per_lap > TREND_DEADBAND && gap < 5.0 => {
                Some((gap / per_lap).ceil() as u32)
            }
            _ => None,
        }
        .filter(|&n| n <= MAX_LAPS_TO_CATCH);
        Some(Trend {
            per_lap,
            from_interval,
            laps_to_catch,
        })
    }
}

/// Racing number of the car one position ahead.
pub fn car_ahead(timing: &Value, line: &Value) -> Option<String> {
    let pos = as_u32(&line["Position"])?;
    if pos <= 1 {
        return None;
    }
    timing["Lines"]
        .as_object()?
        .iter()
        .find(|(_, l)| as_u32(&l["Position"]) == Some(pos - 1))
        .map(|(n, _)| n.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::merge::merge;
    use serde_json::json;

    struct Sim {
        timing: Value,
        laps: LapTracker,
    }

    impl Sim {
        fn new() -> Self {
            let timing = json!({"Lines": {
                "1": {"Position": "1", "NumberOfLaps": 0, "InPit": false, "PitOut": false, "IntervalToPositionAhead": {"Value": "LAP 1"}},
                "4": {"Position": "2", "NumberOfLaps": 0, "InPit": false, "PitOut": false, "IntervalToPositionAhead": {"Value": "+3.000"}},
            }});
            Self {
                timing,
                laps: LapTracker::default(),
            }
        }

        fn delta(&mut self, d: Value) {
            merge(&mut self.timing, d.clone());
            self.laps.on_timing(&d, &self.timing);
        }

        /// Both cars complete lap `n`: the leader in `t1`, car 4 in `t4`.
        fn lap(&mut self, n: u32, t1: f64, t4: f64, interval: f64) {
            self.delta(json!({"Lines": {"1": {"NumberOfLaps": n, "LastLapTime": {"Value": format!("{t1:.3}")}}}}));
            self.delta(json!({"Lines": {"4": {"NumberOfLaps": n, "LastLapTime": {"Value": format!("{t4:.3}")},
                "IntervalToPositionAhead": {"Value": format!("+{interval:.3}")}}}}));
        }

        fn trend(&self, mode: TrendMode) -> Option<Trend> {
            let gap = parse_gap(
                self.timing["Lines"]["4"]["IntervalToPositionAhead"]["Value"]
                    .as_str()
                    .unwrap(),
            );
            self.laps.trend("4", Some("1"), gap, mode)
        }
    }

    #[test]
    fn catching_trend_averages_last_three() {
        let mut s = Sim::new();
        s.lap(1, 90.0, 91.0, 4.0); // lap 1 is skipped
        s.lap(2, 90.0, 89.6, 3.6);
        s.lap(3, 90.0, 89.5, 3.1);
        s.lap(4, 90.0, 89.6, 2.7);
        s.lap(5, 90.0, 89.5, 2.2);
        let t = s.trend(TrendMode::Avg3).unwrap();
        assert!((t.per_lap - 0.4667).abs() < 0.001, "{t:?}");
        assert!(!t.from_interval);
        assert_eq!(t.laps_to_catch, Some(5));
        let last = s.trend(TrendMode::Last).unwrap();
        assert!((last.per_lap - 0.5).abs() < 1e-9);
    }

    #[test]
    fn pit_and_neutral_laps_are_skipped() {
        let mut s = Sim::new();
        s.lap(1, 90.0, 90.0, 3.0);
        s.lap(2, 90.0, 91.0, 4.0);
        s.laps.set_neutral(true);
        s.laps.set_neutral(false);
        s.lap(3, 120.0, 100.0, 1.0); // SC lap: ignored
        assert!((s.trend(TrendMode::Last).unwrap().per_lap + 1.0).abs() < 1e-9);

        s.delta(json!({"Lines": {"1": {"InPit": true}}}));
        s.lap(4, 110.0, 90.0, 1.0); // leader pitted: ignored for lap delta
        s.delta(json!({"Lines": {"1": {"InPit": false}}}));
        let t = s.trend(TrendMode::Last).unwrap();
        assert!((t.per_lap + 1.0).abs() < 1e-9, "{t:?}");
    }

    #[test]
    fn car_ahead_pitting_does_not_show_as_catching() {
        let mut s = Sim::new();
        s.lap(1, 90.0, 90.0, 25.0);
        s.lap(2, 90.0, 90.5, 25.5);
        s.lap(3, 90.0, 90.5, 26.0);
        // The leader's pit stop costs 20 s but it stays ahead: the
        // interval collapses although car 4 is still slower.
        s.delta(json!({"Lines": {"1": {"InPit": true}}}));
        s.lap(4, 110.0, 90.5, 6.5);
        s.delta(json!({"Lines": {"1": {"InPit": false}}}));
        let t = s.trend(TrendMode::Avg3).unwrap();
        assert!((t.per_lap + 0.5).abs() < 1e-9, "{t:?}");
        assert!(!t.from_interval);
    }

    #[test]
    fn pace_over_own_clean_laps() {
        let mut s = Sim::new();
        s.lap(1, 90.0, 95.0, 3.0);
        assert_eq!(s.laps.pace("4", TrendMode::Last), None);
        s.lap(2, 90.0, 91.0, 3.0);
        assert_eq!(s.laps.pace("4", TrendMode::Last), None); // lap 1 skipped
        s.lap(3, 90.0, 90.6, 3.0);
        s.lap(4, 90.0, 90.4, 3.0);
        s.lap(5, 90.0, 90.7, 3.0);
        // 91.0 → 90.7 over 3 laps; last lap 0.3 s slower.
        assert!((s.laps.pace("4", TrendMode::Avg3).unwrap() - 0.1).abs() < 1e-9);
        assert!((s.laps.pace("4", TrendMode::Last).unwrap() + 0.3).abs() < 1e-9);

        // A pit stop starts the run again; the car ahead changing doesn't.
        s.delta(json!({"Lines": {"4": {"InPit": true}}}));
        s.lap(6, 90.0, 110.0, 3.0);
        s.delta(json!({"Lines": {"4": {"InPit": false}}}));
        assert_eq!(s.laps.pace("4", TrendMode::Avg3), None);
        s.delta(
            json!({"Lines": {"1": {"Position": "3"}, "16": {"Position": "1", "NumberOfLaps": 6}}}),
        );
        s.lap(7, 90.0, 95.0, 3.0); // out-lap
        s.lap(8, 90.0, 89.5, 3.0);
        s.lap(9, 90.0, 89.0, 3.0);
        assert!((s.laps.pace("4", TrendMode::Avg3).unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn history_resets_when_car_ahead_changes() {
        let mut s = Sim::new();
        s.lap(1, 90.0, 90.0, 3.0);
        s.lap(2, 90.0, 89.0, 2.0);
        s.lap(3, 90.0, 89.0, 1.0);
        assert!(s.trend(TrendMode::Avg3).is_some());
        // Car 4 is now behind a different car.
        s.delta(
            json!({"Lines": {"1": {"Position": "3"}, "16": {"Position": "1", "NumberOfLaps": 3}}}),
        );
        s.lap(4, 90.0, 89.0, 1.0);
        assert_eq!(
            s.laps
                .trend("4", Some("16"), Gap::Seconds(1.0), TrendMode::Avg3),
            None
        );
    }

    #[test]
    fn late_lap_time_fills_record() {
        let mut s = Sim::new();
        s.lap(1, 90.0, 90.0, 3.0);
        s.lap(2, 90.0, 90.0, 3.0);
        s.delta(json!({"Lines": {"1": {"NumberOfLaps": 3}}}));
        s.delta(json!({"Lines": {"1": {"LastLapTime": {"Value": "1:30.000"}}}}));
        s.delta(json!({"Lines": {"4": {"NumberOfLaps": 3, "IntervalToPositionAhead": {"Value": "+2.000"}}}}));
        s.delta(json!({"Lines": {"4": {"LastLapTime": {"Value": "1:29.000"}}}}));
        assert!((s.trend(TrendMode::Last).unwrap().per_lap - 1.0).abs() < 1e-9);
    }

    #[test]
    fn interval_figure_wins_on_disagreement() {
        let mut s = Sim::new();
        s.lap(1, 90.0, 90.0, 3.0);
        s.lap(2, 90.0, 89.0, 3.0);
        s.lap(3, 90.0, 89.0, 3.0); // lap times say +1.0, interval didn't move
        let t = s.trend(TrendMode::Avg3).unwrap();
        assert!(t.from_interval);
        assert!(t.per_lap.abs() < 1e-9);
    }
}
