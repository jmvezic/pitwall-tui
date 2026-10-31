//! Qualifying: where each car is in its run, when its lap started, the
//! sectors of its best lap in the current part, and its last push lap.
//!
//! F1 resets a car's mini-sectors a few seconds after it crosses the line, so
//! the best lap is copied just before that reset. Mini-sector colours can't
//! tell a push lap from a cool-down (laps after the chequered flag and some
//! out laps are uncoloured), so push laps are told apart by their first
//! sector time, and checked again after the second.

use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::Value;

use super::derive::TimeKind;
use super::merge::indexed;
use super::parse::{as_bool, as_str, as_u32, parse_laptime};

/// A first sector within this factor of the car's best starts a push lap,
/// practice long runs included. Fitted on 25 sessions of 2026 (11 practice
/// sessions, 14 qualifying and sprint qualifying, Monaco and Baku among
/// them): 1.5% of practice laps within 7% of the car's best had a slower
/// first sector (Suzuka, hard on tyres in the esses, 7%), 0.5% in
/// qualifying. Some cool-downs start quicker: laps abandoned after the
/// first sector.
const PUSH_S1: f64 = 1.08;
/// Before the car has a first sector, within this factor of anyone's.
const PUSH_S1_FIELD: f64 = 1.10;
/// The first two sectors within this factor of the car's best keep it a
/// push lap. Over the same sessions, at the line, this leaves 0.2% of
/// practice and 0.5% of qualifying push laps called cool-downs, and 1.3%
/// and 0.7% of cool-downs called push laps.
const PUSH_S12: f64 = 1.10;

/// How long after a new best time a gained position still counts: timing
/// can send the position after the time.
const GAIN_WINDOW: TimeDelta = TimeDelta::seconds(3);

/// What a car on track is doing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// From the pit exit to the line.
    #[default]
    Out,
    Push,
    Cool,
}

/// One sector of a lap: its time once done, and its mini-sectors.
#[derive(Debug, Clone, PartialEq)]
pub struct SectorSnap {
    /// Empty until the sector is done.
    pub value: String,
    pub kind: TimeKind,
    /// F1's mini-sector statuses; 0 is not reached yet.
    pub segments: Vec<u32>,
}

#[derive(Debug, Clone)]
struct BestLap {
    time: String,
    sectors: Vec<SectorSnap>,
}

/// A push lap just over the line.
#[derive(Debug, Clone)]
pub struct Finished {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Copied when the mini-sectors reset; until then the timing line
    /// still holds the lap.
    pub sectors: Option<Vec<SectorSnap>>,
}

#[derive(Debug, Default)]
struct Car {
    lap_start: Option<DateTime<Utc>>,
    phase: Phase,
    /// Whether the lap before the current one was a push lap too: a long
    /// run.
    pushed_before: bool,
    /// The current lap's first sector.
    lap_s1: Option<f64>,
    best_s1: Option<f64>,
    /// Best first and second sectors of one lap.
    best_s12: Option<f64>,
    best: Option<BestLap>,
    finished: Option<Finished>,
    /// Whether the mini-sectors have reset since the lap started: until
    /// then the timing line still shows the lap before.
    cleared: bool,
}

#[derive(Debug, Default)]
pub struct QualiTracker {
    cars: HashMap<String, Car>,
    /// The part the fastest sectors are for.
    part: Option<u32>,
    /// Fastest time in each sector this part, by anyone.
    fastest: Vec<f64>,
    standings: Standings,
}

/// Changes in the standings, for the UI to flash.
#[derive(Debug, Default)]
struct Standings {
    /// Off until the first standings: joining mid-session isn't a change.
    seen: bool,
    leader: Option<String>,
    /// Who took the lead, and when.
    new_leader: Option<(String, DateTime<Utc>)>,
    /// Best time and position at the last update, by car.
    prev: HashMap<String, (String, u32)>,
    /// Cars with a new best time, when and from which position.
    improved: HashMap<String, (DateTime<Utc>, u32)>,
    /// When each car last moved up with a new best time.
    gained: HashMap<String, DateTime<Utc>>,
}

impl QualiTracker {
    /// Takes one car's TimingData delta, before it is merged into `old`.
    /// `part` is the qualifying part (1–3) the timing was in.
    pub fn on_timing(
        &mut self,
        num: &str,
        delta: &Value,
        old: &Value,
        part: Option<u32>,
        now: DateTime<Utc>,
    ) {
        if part != self.part {
            self.part = part;
            self.fastest.clear();
        }
        for (k, s) in delta["Sectors"].as_object().into_iter().flatten() {
            if let (Ok(i), Some(t)) = (k.parse::<usize>(), parse_laptime(as_str(&s["Value"]))) {
                if self.fastest.len() <= i {
                    self.fastest.resize(i + 1, f64::INFINITY);
                }
                self.fastest[i] = self.fastest[i].min(t);
            }
        }

        let car = self.cars.entry(num.to_owned()).or_default();

        let resets = resets_segments(delta);
        if resets {
            // `old` still holds the lap just finished.
            if let Some(f) = &mut car.finished
                && f.sectors.is_none()
            {
                f.sectors = Some(lap_sectors(old));
            }
            // Leaving the pits resets them too, after an in-lap that keeps
            // the last lap time: only a lap whose sectors make it counts.
            let time = as_str(&old["LastLapTime"]["Value"]);
            let sectors = lap_sectors(old);
            if time == part_best(old, part) && adds_up(&sectors, time) {
                car.best = Some(BestLap {
                    time: time.to_owned(),
                    sectors,
                });
            }
        }

        if as_bool(&delta["PitOut"]) == Some(true) {
            car.finished = None;
            car.lap_start = Some(now);
            car.phase = Phase::Out;
            car.pushed_before = false;
            car.lap_s1 = None;
            car.cleared = resets;
        } else if let Some(n) = as_u32(&delta["NumberOfLaps"])
            && as_u32(&old["NumberOfLaps"]).is_some_and(|was| n > was)
        {
            if car.phase == Phase::Push
                && let Some(start) = car.lap_start
            {
                car.finished = Some(Finished {
                    start,
                    end: now,
                    sectors: None,
                });
            }
            car.lap_start = Some(now);
            car.lap_s1 = None;
            car.cleared = resets;
            // A first guess until the first sector says otherwise: a push
            // follows the out lap, a cool-down follows a push, unless the
            // car is on a long run.
            let pushed = car.phase == Phase::Push;
            car.phase = match car.phase {
                Phase::Out | Phase::Cool => Phase::Push,
                Phase::Push if car.pushed_before => Phase::Push,
                Phase::Push => Phase::Cool,
            };
            car.pushed_before = pushed;
        }

        if resets {
            car.cleared = true;
        }

        if let Some(s1) = sector_time(delta, 0) {
            car.lap_s1 = Some(s1);
            if car.phase != Phase::Out {
                let limit = match car.best_s1 {
                    Some(b) => b * PUSH_S1,
                    None => self
                        .fastest
                        .first()
                        .map_or(f64::INFINITY, |f| f * PUSH_S1_FIELD),
                };
                car.phase = if s1 > limit { Phase::Cool } else { Phase::Push };
            }
            car.best_s1 = Some(car.best_s1.map_or(s1, |b| b.min(s1)));
        }
        // A lap abandoned after a quick first sector, or a long run lap
        // slowed in the first.
        if let (Some(s1), Some(s2)) = (car.lap_s1, sector_time(delta, 1)) {
            let s12 = s1 + s2;
            if car.phase != Phase::Out
                && let Some(b) = car.best_s12
            {
                car.phase = if s12 > b * PUSH_S12 {
                    Phase::Cool
                } else {
                    Phase::Push
                };
            }
            car.best_s12 = Some(car.best_s12.map_or(s12, |b| b.min(s12)));
        }
    }

    /// Takes the standings of the current part, in position order, as
    /// (number, position, best time this part).
    pub fn on_standings(&mut self, rows: &[(String, u32, String)], now: DateTime<Utc>) {
        let st = &mut self.standings;
        let leader = rows
            .iter()
            .find(|(_, _, best)| !best.is_empty())
            .map(|(num, ..)| num.clone());
        if st.seen && leader.is_some() && leader != st.leader {
            st.new_leader = leader.clone().map(|num| (num, now));
        }
        st.leader = leader;

        for (num, pos, best) in rows {
            if let Some((was_best, was_pos)) = st.prev.get(num)
                && !best.is_empty()
                && best != was_best
            {
                st.improved.insert(num.clone(), (now, *was_pos));
            }
            st.prev.insert(num.clone(), (best.clone(), *pos));
        }
        st.improved.retain(|num, (at, was_pos)| {
            if now - *at > GAIN_WINDOW {
                return false;
            }
            let moved_up = st.prev.get(num).is_some_and(|(_, pos)| pos < was_pos);
            if moved_up {
                st.gained.insert(num.clone(), now);
            }
            !moved_up
        });
        st.seen = true;
    }

    /// Who last took the lead, and when.
    pub fn new_leader(&self) -> Option<(&str, DateTime<Utc>)> {
        let (num, at) = self.standings.new_leader.as_ref()?;
        Some((num, *at))
    }

    /// When the car last moved up with a new best time.
    pub fn gained(&self, num: &str) -> Option<DateTime<Utc>> {
        self.standings.gained.get(num).copied()
    }

    /// Recolours sector times against this part's fastest: a snapshot keeps
    /// the purple it had when it was set, after someone else went faster.
    pub fn mark_fastest(&self, sectors: &mut [SectorSnap]) {
        for (i, s) in sectors.iter_mut().enumerate() {
            let Some(t) = parse_laptime(&s.value) else {
                continue;
            };
            let fastest = self.fastest.get(i).is_some_and(|&f| t <= f);
            s.kind = match s.kind {
                _ if fastest => TimeKind::Overall,
                // It was the fastest when set, so a personal best too.
                TimeKind::Overall => TimeKind::Personal,
                k => k,
            };
        }
    }

    pub fn phase(&self, num: &str) -> Phase {
        self.cars.get(num).map_or_else(Phase::default, |c| c.phase)
    }

    /// Whether the timing line shows the current lap yet.
    pub fn cleared(&self, num: &str) -> bool {
        self.cars.get(num).is_some_and(|c| c.cleared)
    }

    pub fn lap_start(&self, num: &str) -> Option<DateTime<Utc>> {
        self.cars.get(num)?.lap_start
    }

    /// The car's last push lap, once over the line.
    pub fn finished(&self, num: &str) -> Option<&Finished> {
        self.cars.get(num)?.finished.as_ref()
    }

    /// Sectors of the car's best lap, if `best` (the part's best time, as
    /// timing shows it) is still that lap. A deleted lap no longer matches.
    pub fn best_lap(&self, num: &str, best: &str) -> Option<&[SectorSnap]> {
        let b = self.cars.get(num)?.best.as_ref()?;
        (!best.is_empty() && b.time == best).then_some(b.sectors.as_slice())
    }
}

/// The car's best time in qualifying `part`, or in practice, which has no
/// parts.
pub fn part_best(line: &Value, part: Option<u32>) -> &str {
    let Some(p) = part.filter(|&p| p >= 1) else {
        return as_str(&line["BestLapTime"]["Value"]);
    };
    indexed(&line["BestLapTimes"])
        .get(p as usize - 1)
        .map_or("", |t| as_str(&t["Value"]))
}

/// What a mini-sector says about the sector it's in, as a share of the
/// driver's own best time for that sector, per mini-sector. Coloured ones
/// (`+` or `-`) come with sectors faster than the driver's best lap, grey
/// ones with slower: fitted on every push lap of the 2026 Monza and
/// Singapore qualifying sessions.
const MINI_COLOURED: f64 = -0.020;
const MINI_GREY: f64 = 0.0165;

/// The lap time `sectors` (a lap in progress) is heading for: the sectors
/// done, then the driver's own best lap (`own`) for the rest, adjusted in
/// the sector being driven by the mini-sectors passed. Without an own best
/// lap, P1's lap for the rest, slowed by how far behind it the lap is so
/// far.
pub fn project(
    sectors: &[SectorSnap],
    own: &[SectorSnap],
    p1: Option<&[SectorSnap]>,
) -> Option<f64> {
    let done: Vec<f64> = sectors
        .iter()
        .map_while(|s| parse_laptime(&s.value))
        .collect();
    let current = sectors.get(done.len());
    let passed = current.map_or(0, |s| s.segments.iter().filter(|&&g| g != 0).count());
    if done.is_empty() && passed == 0 {
        return None;
    }
    let so_far: f64 = done.iter().sum();

    let own: Option<Vec<f64>> = own.iter().map(|s| parse_laptime(&s.value)).collect();
    if let Some(own) = own.filter(|o| o.len() == sectors.len()) {
        let mut rest: f64 = own[done.len()..].iter().sum();
        if let Some(s) = current {
            let n = s.segments.len() as f64;
            let share: f64 = s
                .segments
                .iter()
                .filter(|&&g| g != 0 && g & 16 == 0)
                .map(|&g| if g & 3 == 0 { MINI_GREY } else { MINI_COLOURED })
                .sum();
            rest += own[done.len()] * share / n;
        }
        return Some(so_far + rest);
    }

    let p1: Vec<f64> = p1?
        .iter()
        .map(|s| parse_laptime(&s.value))
        .collect::<Option<_>>()?;
    let p1_so_far: f64 = p1.get(..done.len())?.iter().sum();
    (!done.is_empty() && p1.len() == sectors.len())
        .then(|| so_far + p1[done.len()..].iter().sum::<f64>() * so_far / p1_so_far)
}

/// Whether the sector times make up `lap`, to the rounding of the feed.
pub fn adds_up(sectors: &[SectorSnap], lap: &str) -> bool {
    let Some(lap) = parse_laptime(lap) else {
        return false;
    };
    let sum: Option<f64> = sectors.iter().map(|s| parse_laptime(&s.value)).sum();
    sum.is_some_and(|sum| (sum - lap).abs() < 0.005)
}

/// A sector's time in a TimingData delta, whichever way its sectors come.
fn sector_time(delta: &Value, i: usize) -> Option<f64> {
    let s = &delta["Sectors"];
    parse_laptime(as_str(&s[i.to_string()]["Value"]))
        .or_else(|| parse_laptime(as_str(&s[i]["Value"])))
}

/// The start of a new lap clears the mini-sectors to 0.
fn resets_segments(delta: &Value) -> bool {
    indexed(&delta["Sectors"]).iter().any(|s| {
        indexed(&s["Segments"])
            .iter()
            .any(|g| as_u32(&g["Status"]) == Some(0))
    })
}

/// The lap in a timing line: each sector's mini-sectors, and its time once
/// all of them are reached. Until then the time is the previous lap's.
pub fn lap_sectors(line: &Value) -> Vec<SectorSnap> {
    indexed(&line["Sectors"])
        .iter()
        .map(|s| {
            let segments: Vec<u32> = indexed(&s["Segments"])
                .iter()
                .map(|g| as_u32(&g["Status"]).unwrap_or(0))
                .collect();
            let done = !segments.is_empty() && segments.iter().all(|&g| g != 0);
            let value = as_str(&s["Value"]);
            let (value, kind) = if done && !value.is_empty() {
                let kind = if as_bool(&s["OverallFastest"]) == Some(true) {
                    TimeKind::Overall
                } else if as_bool(&s["PersonalFastest"]) == Some(true) {
                    TimeKind::Personal
                } else {
                    TimeKind::Normal
                };
                (value.to_owned(), kind)
            } else {
                (String::new(), TimeKind::Normal)
            };
            SectorSnap {
                value,
                kind,
                segments,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use serde_json::json;

    fn t(s: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + s, 0).unwrap()
    }

    fn line(laps: u32, last: &str, best: &str, seg: u32) -> Value {
        json!({
            "NumberOfLaps": laps,
            "LastLapTime": {"Value": last},
            "BestLapTimes": [{"Value": best}],
            "Sectors": [
                {"Value": "27.0", "PersonalFastest": true, "Segments": [{"Status": seg}, {"Status": seg}]},
                {"Value": "28.0", "OverallFastest": true, "Segments": [{"Status": seg}]},
            ],
        })
    }

    #[test]
    fn a_run() {
        let mut q = QualiTracker::default();
        let p = Some(1);
        // Out of the pits.
        q.on_timing(
            "1",
            &json!({"PitOut": true, "NumberOfLaps": 1}),
            &line(0, "", "", 0),
            p,
            t(0),
        );
        assert_eq!((q.phase("1"), q.lap_start("1")), (Phase::Out, Some(t(0))));

        // Over the line: a push lap.
        q.on_timing(
            "1",
            &json!({"NumberOfLaps": 2}),
            &line(1, "", "", 2048),
            p,
            t(90),
        );
        assert_eq!(q.phase("1"), Phase::Push);
        // The out lap shows until the mini-sectors reset.
        assert!(!q.cleared("1"));
        q.on_timing(
            "1",
            &json!({"Sectors": {"0": {"Segments": {"0": {"Status": 0}}}}}),
            &line(2, "", "", 2048),
            p,
            t(94),
        );
        assert!(q.cleared("1"));
        q.on_timing(
            "1",
            &json!({"Sectors": {"0": {"Value": "27.0"}}}),
            &line(2, "", "", 0),
            p,
            t(117),
        );
        assert_eq!(q.phase("1"), Phase::Push);

        // It sets the best time; then the mini-sectors reset.
        q.on_timing(
            "1",
            &json!({"NumberOfLaps": 3}),
            &line(2, "", "", 2051),
            p,
            t(172),
        );
        assert_eq!(q.phase("1"), Phase::Cool);
        let f = q.finished("1").unwrap();
        assert_eq!((f.start, f.end, f.sectors.is_none()), (t(90), t(172), true));
        let done = line(3, "55.000", "55.000", 2051);
        q.on_timing(
            "1",
            &json!({"Sectors": {"0": {"Segments": {"0": {"Status": 0}}}}}),
            &done,
            p,
            t(176),
        );
        assert_eq!(
            q.finished("1").unwrap().sectors.as_ref().unwrap()[1].value,
            "28.0"
        );
        let best = q.best_lap("1", "55.000").unwrap();
        assert_eq!(best[0].segments, [2051, 2051]);
        assert_eq!(
            (best[1].value.as_str(), best[1].kind),
            ("28.0", TimeKind::Overall)
        );
        // A deleted lap no longer matches the best time.
        assert!(q.best_lap("1", "").is_none());

        // A slow first sector: cooling down, as guessed.
        q.on_timing(
            "1",
            &json!({"Sectors": {"0": {"Value": "31.5"}}}),
            &done,
            p,
            t(203),
        );
        assert_eq!(q.phase("1"), Phase::Cool);
        // The next lap is guessed a push, but its first sector is slow too.
        q.on_timing(
            "1",
            &json!({"NumberOfLaps": 4}),
            &done,
            p,
            t(240) + TimeDelta::zero(),
        );
        assert_eq!(q.phase("1"), Phase::Push);
        q.on_timing(
            "1",
            &json!({"Sectors": {"0": {"Value": "30.0"}}}),
            &done,
            p,
            t(270),
        );
        assert_eq!(q.phase("1"), Phase::Cool);
    }

    /// Mexico City FP1 2026: Leclerc's and Pérez's long runs, 3–5% off
    /// their best first sector.
    #[test]
    fn long_run_laps_are_pushes() {
        let mut q = QualiTracker::default();
        let old = line(1, "", "", 2049);
        let lap = |n: u32| json!({"NumberOfLaps": n});
        let sectors = |s1: &str, s2: &str| {
            [
                json!({"Sectors": {"0": {"Value": s1}}}),
                json!({"Sectors": {"1": {"Value": s2}}}),
            ]
        };
        q.on_timing("16", &json!({"PitOut": true}), &old, None, t(0));
        let mut n = 1;
        let mut lap_of = |q: &mut QualiTracker, s1: &str, s2: &str| {
            n += 1;
            q.on_timing("16", &lap(n), &old, None, t(n as i64 * 80));
            let guess = q.phase("16");
            for d in sectors(s1, s2) {
                q.on_timing("16", &d, &old, None, t(n as i64 * 80 + 30));
            }
            (guess, q.phase("16"))
        };
        assert_eq!(lap_of(&mut q, "27.065", "26.0"), (Phase::Push, Phase::Push));
        // The first lap of the run is guessed a cool-down.
        assert_eq!(lap_of(&mut q, "28.354", "26.9"), (Phase::Cool, Phase::Push));
        // From then on a push, as guessed.
        assert_eq!(lap_of(&mut q, "28.023", "26.8"), (Phase::Push, Phase::Push));
        // The end of the run.
        assert_eq!(lap_of(&mut q, "34.924", "35.0"), (Phase::Push, Phase::Cool));
        assert_eq!(lap_of(&mut q, "40.0", "40.0"), (Phase::Push, Phase::Cool));
    }

    #[test]
    fn lap_abandoned_after_the_first_sector() {
        let mut q = QualiTracker::default();
        let old = line(1, "", "", 2049);
        let s = |i: &str, v: &str| json!({"Sectors": {i: {"Value": v}}});
        q.on_timing("1", &s("0", "27.0"), &old, None, t(0));
        q.on_timing("1", &s("1", "26.0"), &old, None, t(30));
        q.on_timing("1", &json!({"NumberOfLaps": 2}), &old, None, t(80));
        q.on_timing("1", &json!({"NumberOfLaps": 3}), &old, None, t(160));
        q.on_timing("1", &s("0", "27.2"), &old, None, t(190));
        assert_eq!(q.phase("1"), Phase::Push);
        q.on_timing("1", &s("1", "33.0"), &old, None, t(225));
        assert_eq!(q.phase("1"), Phase::Cool);
    }

    #[test]
    fn first_push_judged_against_the_field() {
        let mut q = QualiTracker::default();
        let old = line(1, "", "", 2049);
        let s1 = |v: &str| json!({"Sectors": {"0": {"Value": v}}});
        let lap = |n: u32| json!({"NumberOfLaps": n});
        q.on_timing("1", &s1("27.0"), &old, Some(1), t(0));
        for (num, s, want) in [("2", "29.6", Phase::Push), ("3", "29.8", Phase::Cool)] {
            q.on_timing(num, &json!({"PitOut": true}), &old, Some(1), t(0));
            q.on_timing(num, &lap(2), &old, Some(1), t(90));
            q.on_timing(num, &s1(s), &old, Some(1), t(120));
            assert_eq!(q.phase(num), want, "{num}");
        }
    }

    #[test]
    fn standings_changes() {
        let mut q = QualiTracker::default();
        let rows = |r: &[(&str, u32, &str)]| -> Vec<(String, u32, String)> {
            r.iter()
                .map(|&(n, p, b)| (n.to_owned(), p, b.to_owned()))
                .collect()
        };
        // Joining mid-session flashes nothing.
        q.on_standings(
            &rows(&[("1", 1, "1:22.0"), ("2", 2, "1:23.0"), ("3", 3, "")]),
            t(0),
        );
        assert!(q.new_leader().is_none());

        // A better time without moving up.
        q.on_standings(
            &rows(&[("1", 1, "1:22.0"), ("2", 2, "1:22.5"), ("3", 3, "")]),
            t(10),
        );
        assert!(q.gained("2").is_none());

        // A first time, with the position a message later.
        q.on_standings(
            &rows(&[("1", 1, "1:22.0"), ("2", 2, "1:22.5"), ("3", 3, "1:22.9")]),
            t(20),
        );
        assert!(q.gained("3").is_none());
        q.on_standings(
            &rows(&[("1", 1, "1:22.0"), ("3", 2, "1:22.4"), ("2", 3, "1:22.5")]),
            t(21),
        );
        q.on_standings(
            &rows(&[("1", 1, "1:22.0"), ("3", 2, "1:22.4"), ("2", 3, "1:22.5")]),
            t(21),
        );
        assert_eq!(q.gained("3"), Some(t(21)));

        // A new leader.
        q.on_standings(
            &rows(&[("2", 1, "1:21.9"), ("1", 2, "1:22.0"), ("3", 3, "1:22.4")]),
            t(30),
        );
        assert_eq!(q.new_leader(), Some(("2", t(30))));
        assert_eq!(q.gained("2"), Some(t(30)));
        // Improving on its own lead isn't a new one.
        q.on_standings(
            &rows(&[("2", 1, "1:21.8"), ("1", 2, "1:22.0"), ("3", 3, "1:22.4")]),
            t(40),
        );
        assert_eq!(q.new_leader(), Some(("2", t(30))));

        // A new part: the first time set leads.
        q.on_standings(&rows(&[("2", 1, ""), ("1", 2, ""), ("3", 3, "")]), t(600));
        q.on_standings(
            &rows(&[("3", 1, "1:21.5"), ("2", 2, ""), ("1", 3, "")]),
            t(700),
        );
        assert_eq!(q.new_leader(), Some(("3", t(700))));
    }

    #[test]
    fn projects_from_own_best_lap() {
        let sector = |value: &str, segments: &[u32]| SectorSnap {
            value: value.to_owned(),
            kind: TimeKind::Normal,
            segments: segments.to_vec(),
        };
        let own = [
            sector("27.743", &[2049; 4]),
            sector("39.222", &[2049; 4]),
            sector("26.200", &[2049; 4]),
        ];
        // Singapore Q2, HUL: S1 done, nothing of S2 yet.
        let lap = [
            sector("27.578", &[2049; 4]),
            sector("", &[0; 4]),
            sector("", &[0; 4]),
        ];
        let p = project(&lap, &own, None).unwrap();
        assert!((p - (27.578 + 39.222 + 26.200)).abs() < 1e-9);

        // Half of S2, coloured: faster than the own best there.
        let lap = [
            sector("27.578", &[2049; 4]),
            sector("", &[2049, 2051, 0, 0]),
            sector("", &[0; 4]),
        ];
        let p = project(&lap, &own, None).unwrap();
        let want = 27.578 + 39.222 * (1.0 + 2.0 * MINI_COLOURED / 4.0) + 26.200;
        assert!((p - want).abs() < 1e-9);
        // Grey: slower.
        let lap = [
            sector("27.578", &[2049; 4]),
            sector("", &[2048, 2048, 0, 0]),
            sector("", &[0; 4]),
        ];
        assert!(project(&lap, &own, None).unwrap() > 27.578 + 39.222 + 26.200);

        // No own best: P1's lap, slowed by the gap so far.
        let none = [sector("", &[]), sector("", &[]), sector("", &[])];
        let p1 = [
            sector("27.260", &[]),
            sector("38.543", &[]),
            sector("25.963", &[]),
        ];
        let lap = [
            sector("27.578", &[2049; 4]),
            sector("", &[0; 4]),
            sector("", &[0; 4]),
        ];
        let p = project(&lap, &none, Some(&p1)).unwrap();
        assert!((p - (27.578 + (38.543 + 25.963) * 27.578 / 27.260)).abs() < 1e-9);
        // Nothing to go on before the first sector.
        let lap = [
            sector("", &[2049, 0, 0, 0]),
            sector("", &[0; 4]),
            sector("", &[0; 4]),
        ];
        assert!(project(&lap, &none, Some(&p1)).is_none());
        assert!(project(&lap, &own, None).is_some());
    }

    #[test]
    fn in_lap_is_not_saved_as_the_best_lap() {
        let mut q = QualiTracker::default();
        let mut in_lap = line(5, "55.000", "55.000", 2048);
        in_lap["Sectors"][0]["Value"] = json!("33.1");
        // Leaving the pits resets the mini-sectors.
        q.on_timing(
            "1",
            &json!({"PitOut": true, "Sectors": {"0": {"Segments": {"0": {"Status": 0}}}}}),
            &in_lap,
            Some(1),
            t(0),
        );
        assert!(q.best_lap("1", "55.000").is_none());
    }

    #[test]
    fn purple_only_while_fastest() {
        let mut q = QualiTracker::default();
        let old = line(1, "", "", 2049);
        let s1 = |v: &str| json!({"Sectors": {"0": {"Value": v}}});
        q.on_timing("1", &s1("27.0"), &old, Some(1), t(0));
        q.on_timing("2", &s1("26.9"), &old, Some(1), t(1));
        let mut snap = lap_sectors(&old);
        snap[0].kind = TimeKind::Overall;
        q.mark_fastest(&mut snap);
        assert_eq!(snap[0].kind, TimeKind::Personal);

        // A new part starts from scratch.
        q.on_timing("1", &s1("27.0"), &old, Some(2), t(600));
        q.mark_fastest(&mut snap);
        assert_eq!(snap[0].kind, TimeKind::Overall);
    }

    #[test]
    fn sector_time_waits_for_its_mini_sectors() {
        let mut l = line(1, "", "", 2049);
        l["Sectors"][1]["Segments"][0]["Status"] = json!(0);
        let s = lap_sectors(&l);
        assert_eq!(
            (s[0].value.as_str(), s[0].kind),
            ("27.0", TimeKind::Personal)
        );
        assert_eq!(s[1].value, "");
    }
}
