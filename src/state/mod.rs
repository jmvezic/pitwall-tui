//! Session state: raw merged topics plus the views derived from them.
//!
//! Each topic is kept as a raw `serde_json::Value` with every delta
//! deep-merged in, so new or renamed fields don't break parsing. Typed
//! views are built from the merged values in [`derive`].

#[cfg(test)]
mod archive_tests;
pub mod derive;
pub mod laps;
pub mod merge;
pub mod parse;
pub mod quali;
pub mod title;
pub mod track;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Map, Value};

use crate::feed::{FeedMessage, decode_z};
use crate::season::Season;
use derive::{Changed, TrackState, Views, parse_utc};
use laps::LapTracker;
use parse::{as_bool, as_str, as_u32};
use track::{CarPos, Cars, OutlineBuilder, StopTracker};

/// A grid procedure longer than this is given up on, so the stopped list
/// can't stay frozen.
const GRID_TIMEOUT: TimeDelta = TimeDelta::minutes(10);

/// Where a start or standing restart is. While one is under way cars
/// stand still on the grid, which isn't stopping on track.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum StartPhase {
    #[default]
    Racing,
    /// Extra formation lap or aborted start: the field moves off first.
    AwaitMoving(DateTime<Utc>),
    /// Formation lap, or cars pulling into their grid slots.
    AwaitGrid(DateTime<Utc>),
    /// Most of the field is still: waiting for lights out.
    OnGrid(DateTime<Utc>),
}

/// What the cars on track are doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    /// At least half of them (and at least 6) stand still: a grid.
    Still,
    /// Fewer than a quarter stand still.
    Moving,
    /// Cars pulling up or moving off.
    Between,
    /// No telemetry.
    Unknown,
}

/// A car stopped on track this long is shown as retired.
pub const RETIRE_AFTER: TimeDelta = TimeDelta::minutes(2);

#[derive(Debug, Default)]
pub struct State {
    pub topics: Map<String, Value>,
    pub cars: Cars,
    pub laps: LapTracker,
    pub stops: StopTracker,
    /// Cars stopped on track for longer than [`RETIRE_AFTER`], or marked
    /// `Retired`. Sticky: the feed only sets `Retired` after the race, and
    /// `Stopped` stays true after a crash.
    pub retired: HashSet<String>,
    /// When each car last entered the pit lane, to withdraw a black and
    /// orange flag.
    pub pit_entry: HashMap<String, DateTime<Utc>>,
    grid: StartPhase,
    /// Lights out of the last start or restart. Telemetry stops only count
    /// after it.
    grid_until: Option<DateTime<Utc>>,
    pub outline: OutlineBuilder,
    pub quali: quali::QualiTracker,
    /// The weather sample before the current one, for its changes.
    weather_prev: Value,
    /// Per driver, the stint index the feed last updated. Usually the last
    /// stint, but F1 sometimes rewrites history and leaves a stale entry
    /// at the end (Belgium 2025).
    active_stint: HashMap<String, usize>,
    pub views: Views,
    /// Latest Heartbeat: (stream offset, UTC), to map offsets to time.
    heartbeat: Option<(Duration, DateTime<Utc>)>,
    /// Feed time of the last message applied.
    pub feed_utc: Option<DateTime<Utc>>,
    /// The championship around this session, from Jolpica. Kept through
    /// resets.
    pub season: Option<Season>,
    /// Drivers who could secure the title this session, kept after they
    /// no longer can.
    title_contenders: Vec<String>,
    /// The driver who'd have the title if the session finished now, and
    /// since when.
    title_secured: Option<(String, DateTime<Utc>)>,
    /// When each driver was last given a penalty, by racing number: when
    /// the message came in, a couple of seconds after the time it carries.
    penalised: HashMap<String, DateTime<Utc>>,
}

impl State {
    /// UTC at a stream offset, from the latest heartbeat.
    pub fn utc_at(&self, offset: Duration) -> Option<DateTime<Utc>> {
        let (off, utc) = self.heartbeat?;
        let d = TimeDelta::from_std(offset).ok()? - TimeDelta::from_std(off).ok()?;
        Some(utc + d)
    }

    fn now(&self) -> DateTime<Utc> {
        self.feed_utc.unwrap_or_else(Utc::now)
    }

    /// Clears everything the feed brought, keeping the season.
    pub fn reset(&mut self) {
        *self = State {
            season: self.season.take(),
            ..State::default()
        };
    }

    /// Who'd have the title if the session finished now, and since when.
    pub fn title_secured(&self) -> Option<(&str, DateTime<Utc>)> {
        self.title_secured.as_ref().map(|(n, t)| (n.as_str(), *t))
    }

    /// When each driver was last given a penalty, by racing number.
    pub fn penalised(&self) -> &HashMap<String, DateTime<Utc>> {
        &self.penalised
    }

    /// Replaces everything with a subscribe snapshot.
    pub fn apply_snapshot(&mut self, snap: Map<String, Value>) -> Changed {
        self.reset();
        let mut z = Vec::new();
        for (topic, data) in snap {
            if topic.ends_with(".z") {
                z.push((topic, data));
                continue;
            }
            let mut v = Value::Null;
            merge::merge(&mut v, data);
            self.topics.insert(topic, v);
        }
        if let Some(utc) = parse_utc(as_str(&self.topics_value("Heartbeat")["Utc"])) {
            self.feed_utc = Some(utc);
        }
        for (topic, data) in z {
            self.apply_compressed(&topic, &data);
        }
        let neutral = self.track_state().neutralised();
        let timing = self.topics.get("TimingData").unwrap_or(&Value::Null);
        self.laps.init_from(timing, neutral);
        self.eval_all_stops();
        Changed::all()
    }

    pub fn apply(&mut self, msg: FeedMessage) -> Changed {
        let FeedMessage {
            topic,
            data,
            offset,
            utc,
        } = msg;
        if topic == "Heartbeat"
            && let Some(hb) = parse_utc(as_str(&data["Utc"]))
        {
            self.heartbeat = Some((offset, hb));
        }
        if let Some(t) = utc.or_else(|| self.utc_at(offset)) {
            self.feed_utc = Some(t);
        }

        if topic.ends_with(".z") {
            return self.apply_compressed(&topic, &data);
        }
        if topic == "Position" || topic == "CarData" {
            return self.apply_telemetry(&topic, &data);
        }

        if topic == "TimingAppData" {
            self.note_active_stints(&data);
        }
        let delta = (topic == "TimingData").then(|| data.clone());
        let delta_rcm = if topic == "RaceControlMessages" {
            data.clone()
        } else {
            Value::Null
        };
        // Lights out at the start; the formation lap at a resumption.
        if topic == "SessionStatus" && as_str(&data["Status"]) == "Started" {
            self.grid = StartPhase::AwaitGrid(self.now());
        }
        if let Some(lines) = delta.as_ref().and_then(|d| d["Lines"].as_object()) {
            let now = self.now();
            // Not merged yet: the values before this delta.
            let old_timing = self.topics.get("TimingData").unwrap_or(&Value::Null);
            let old = &old_timing["Lines"];
            let part = as_u32(&old_timing["SessionPart"]);
            for (num, l) in lines {
                self.quali.on_timing(num, l, &old[num], part, now);
                if as_bool(&l["InPit"]) == Some(true) {
                    self.pit_entry.insert(num.clone(), now);
                }
                // Set for cars that didn't finish, once the race is over,
                // and occasionally for a car in the garage that then
                // rejoins: F1 clears it again (Zandvoort sprint 2026, PER).
                match as_bool(&l["Retired"]) {
                    Some(true) => {
                        self.retired.insert(num.clone());
                    }
                    Some(false) => {
                        self.retired.remove(num);
                    }
                    None => {}
                }
                // Completing a lap means it's still racing, whatever made
                // us think it retired.
                if let Some(n) = as_u32(&l["NumberOfLaps"])
                    && as_u32(&old[num]["NumberOfLaps"]).is_some_and(|was| n > was)
                {
                    self.retired.remove(num);
                }
            }
        }
        if topic == "WeatherData"
            && let Some(cur) = self.topics.get("WeatherData")
        {
            self.weather_prev = cur.clone();
        }
        match self.topics.get_mut(&topic) {
            Some(v) => merge::merge(v, data),
            None => {
                let mut v = Value::Null;
                merge::merge(&mut v, data);
                self.topics.insert(topic.clone(), v);
            }
        }

        let mut ch = Changed::default();
        match topic.as_str() {
            "TimingData" => {
                let delta = delta.unwrap_or_default();
                ch.tower = touches_tower(&delta);
                // Mini-sector progress moves cars along the track line.
                ch.map = true;
                let timing = self.topics.get("TimingData").unwrap_or(&Value::Null);
                for num in self.laps.on_timing(&delta, timing) {
                    let clean = self.laps.last_valid(&num);
                    self.outline.on_lap(&num, clean);
                }
                if delta.get("SessionPart").is_some() {
                    ch.session = true;
                }
                if let Some(lines) = delta["Lines"].as_object() {
                    let nums: Vec<String> = lines.keys().cloned().collect();
                    for n in nums {
                        ch.stops |= self.eval_stop(&n);
                    }
                }
            }
            "TrackStatus" => {
                ch.track = true;
                let st = self.track_state();
                self.laps.set_neutral(st.neutralised());
                ch.stops |= self.eval_all_stops();
            }
            "SessionStatus" => {
                ch.session = true;
                ch.stops |= self.eval_all_stops();
            }
            "RaceControlMessages" => {
                ch.rcm = true;
                let now = self.now();
                for m in merge::indexed(&delta_rcm["Messages"]) {
                    match as_str(&m["Message"]) {
                        "STANDING START" => self.grid = StartPhase::AwaitGrid(now),
                        "EXTRA FORMATION LAP" | "ABORTED START" => {
                            self.grid = StartPhase::AwaitMoving(now)
                        }
                        text => {
                            if let Some((false, _, num)) = derive::parse_penalty(text) {
                                self.penalised.insert(num, now);
                            }
                        }
                    }
                }
            }
            "TimingAppData" | "DriverList" | "ChampionshipPrediction" => ch.tower = true,
            "WeatherData" => ch.weather = true,
            "SessionInfo" | "LapCount" | "ExtrapolatedClock" => ch.session = true,
            _ => {}
        }
        ch.tower |= self.retire_long_stops();
        ch
    }

    /// Moves cars stopped for longer than [`RETIRE_AFTER`] to `retired`.
    fn retire_long_stops(&mut self) -> bool {
        let now = self.now();
        let long: Vec<String> = self
            .stops
            .stopped
            .iter()
            .filter(|(n, s)| now - s.since > RETIRE_AFTER && !self.retired.contains(*n))
            .map(|(n, _)| n.clone())
            .collect();
        let changed = !long.is_empty();
        self.retired.extend(long);
        changed
    }

    fn apply_compressed(&mut self, topic: &str, data: &Value) -> Changed {
        let base = topic.trim_end_matches(".z");
        match decode_z(data) {
            Ok(v) => self.apply_telemetry(base, &v),
            Err(e) => {
                tracing::warn!("{topic}: {e:#}");
                Changed::default()
            }
        }
    }

    /// Position: keep recent samples per car. CarData: keep recent speeds.
    fn apply_telemetry(&mut self, topic: &str, v: &Value) -> Changed {
        let mut ch = Changed::default();
        match topic {
            "Position" => {
                for sample in merge::indexed(&v["Position"]) {
                    let Some(ts) = parse_utc(as_str(&sample["Timestamp"])) else {
                        continue;
                    };
                    let Some(entries) = sample["Entries"].as_object() else {
                        continue;
                    };
                    for (num, e) in entries {
                        let (Some(x), Some(y)) = (e["X"].as_f64(), e["Y"].as_f64()) else {
                            continue;
                        };
                        self.cars.push_pos(
                            num,
                            CarPos {
                                ts,
                                x,
                                y,
                                on_track: as_str(&e["Status"]) != "OffTrack",
                            },
                        );
                        self.outline.on_position(num, x, y);
                    }
                }
                ch.map = true;
            }
            "CarData" => {
                let mut nums = Vec::new();
                for entry in merge::indexed(&v["Entries"]) {
                    let Some(ts) = parse_utc(as_str(&entry["Utc"])) else {
                        continue;
                    };
                    let Some(cars) = entry["Cars"].as_object() else {
                        continue;
                    };
                    for (num, car) in cars {
                        if let Some(kmh) = car["Channels"]["2"].as_f64() {
                            self.cars.push_speed(num, ts, kmh);
                            nums.push(num.clone());
                        }
                    }
                }
                nums.sort();
                nums.dedup();
                for n in nums {
                    ch.stops |= self.eval_stop(&n);
                }
            }
            _ => {}
        }
        ch
    }

    fn note_active_stints(&mut self, delta: &Value) {
        let Some(lines) = delta["Lines"].as_object() else {
            return;
        };
        for (num, line) in lines {
            let latest = match &line["Stints"] {
                Value::Object(m) => m
                    .iter()
                    .filter(|(_, s)| s.get("TotalLaps").is_some() || s.get("Compound").is_some())
                    .filter_map(|(k, _)| k.parse::<usize>().ok())
                    .max(),
                Value::Array(a) => a.len().checked_sub(1),
                _ => None,
            };
            if let Some(i) = latest {
                self.active_stint.insert(num.clone(), i);
            }
        }
    }

    /// Index of the driver's current stint, if the feed has told us.
    pub fn active_stint(&self, num: &str) -> Option<usize> {
        self.active_stint.get(num).copied()
    }

    fn topics_value(&self, topic: &str) -> &Value {
        self.topics.get(topic).unwrap_or(&Value::Null)
    }

    pub fn track_state(&self) -> TrackState {
        TrackState::parse(as_str(&self.topics_value("TrackStatus")["Status"]))
    }

    fn eval_all_stops(&mut self) -> bool {
        let mut nums: Vec<String> = self.topics_value("TimingData")["Lines"]
            .as_object()
            .map(|l| l.keys().cloned().collect())
            .unwrap_or_default();
        nums.extend(self.stops.stopped.keys().cloned());
        nums.sort();
        nums.dedup();
        let mut changed = false;
        for n in nums {
            changed |= self.eval_stop(&n);
        }
        changed
    }

    /// Moves the grid procedure on from what the field is doing.
    fn update_grid(&mut self) -> StartPhase {
        let now = self.now();
        let field = self.field();
        self.grid = match (self.grid, field) {
            (StartPhase::Racing, _) => StartPhase::Racing,
            // Telemetry can be missing for a while, for example after a
            // replay seek skips old samples, so only the timeout gives up.
            (StartPhase::AwaitMoving(t) | StartPhase::AwaitGrid(t) | StartPhase::OnGrid(t), _)
                if now - t > GRID_TIMEOUT =>
            {
                self.grid_until = Some(now);
                StartPhase::Racing
            }
            (g, Field::Unknown) => g,
            (StartPhase::AwaitMoving(t), Field::Moving) => StartPhase::AwaitGrid(t),
            (StartPhase::AwaitGrid(t), Field::Still) => StartPhase::OnGrid(t),
            (StartPhase::OnGrid(_), Field::Moving) => {
                self.grid_until = Some(now);
                StartPhase::Racing
            }
            (g, _) => g,
        };
        self.grid
    }

    /// What the cars on track are doing, from telemetry.
    fn field(&self) -> Field {
        let Some(lines) = self.topics_value("TimingData")["Lines"].as_object() else {
            return Field::Unknown;
        };
        let (mut still, mut known) = (0, 0);
        for (num, l) in lines {
            let flag = |k: &str| as_bool(&l[k]) == Some(true);
            if flag("InPit") || flag("PitOut") || flag("Retired") || self.retired.contains(num) {
                continue;
            }
            match self.cars.stationary(num, None) {
                Some(true) => {
                    still += 1;
                    known += 1;
                }
                Some(false) => known += 1,
                None => {}
            }
        }
        if known == 0 {
            Field::Unknown
        } else if still >= 6 && still * 2 >= known {
            Field::Still
        } else if still * 4 < known {
            Field::Moving
        } else {
            Field::Between
        }
    }

    /// Stopped on track: session running, not red-flagged, not in the pit
    /// lane, and either flagged `Stopped` by timing or stationary per
    /// telemetry.
    fn eval_stop(&mut self, num: &str) -> bool {
        // Under a red flag every car stops: leave the list as it was.
        if self.track_state() == TrackState::Red {
            return false;
        }
        // Cars on the grid, before a start or a standing restart, stand
        // still too; timing's own flag doesn't fire for them.
        let on_grid = self.update_grid() != StartPhase::Racing;
        let line = &self.topics_value("TimingData")["Lines"][num];
        let running = as_str(&self.topics_value("SessionStatus")["Status"]) == "Started";
        let flag = |k: &str| as_bool(&line[k]) == Some(true);
        let stopped = running
            && !line.is_null()
            && !flag("InPit")
            && !flag("PitOut")
            && (flag("Stopped")
                || (!on_grid && self.cars.stationary(num, self.grid_until) == Some(true)));
        let lap = as_u32(&line["NumberOfLaps"]).unwrap_or(0) + 1;
        let now = self.now();
        self.stops.update(num, stopped, now, lap)
    }
}

/// Most TimingData deltas are mini-sector and speed-trap updates the tower
/// doesn't show; skip redrawing for those.
fn touches_tower(delta: &Value) -> bool {
    let Some(top) = delta.as_object() else {
        return true;
    };
    top.iter().any(|(k, v)| {
        k != "Lines"
            || v.as_object().is_none_or(|lines| {
                lines.values().any(|l| {
                    l.as_object()
                        .is_none_or(|l| l.keys().any(|k| k != "Sectors" && k != "Speeds"))
                })
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(secs: u64, topic: &str, data: Value) -> FeedMessage {
        FeedMessage {
            topic: topic.into(),
            data,
            offset: Duration::from_secs(secs),
            utc: None,
        }
    }

    #[test]
    fn weather_changes_since_last_sample() {
        let mut s = State::default();
        let sample = |air: &str, rain: &str| {
            json!({"AirTemp": air, "Humidity": "38.5", "Rainfall": rain, "TrackTemp": "55.0",
                   "WindDirection": "354", "WindSpeed": "2.9"})
        };
        let ch = s.apply(msg(0, "WeatherData", sample("31.5", "0")));
        s.derive(ch, laps::TrendMode::Avg3);
        let w = s.views.weather.unwrap();
        assert_eq!((w.air.value, w.air.delta, w.rain), (31.5, None, false));
        assert_eq!(derive::compass(w.wind_dir.unwrap()), "N");

        let ch = s.apply(msg(60, "WeatherData", sample("31.2", "1")));
        s.derive(ch, laps::TrendMode::Avg3);
        let w = s.views.weather.unwrap();
        assert!((w.air.delta.unwrap() + 0.3).abs() < 1e-9);
        assert_eq!(w.track.delta, Some(0.0));
        assert!(w.rain);
    }

    #[test]
    fn penalised_when_the_message_comes_in() {
        let mut s = State::default();
        s.apply(msg(0, "Heartbeat", json!({"Utc": "2026-10-25T20:00:00Z"})));
        // Stamped two seconds before it arrives, as race control's are.
        let rcm = |i: u32, utc: &str, text: &str| json!({"Messages": {i.to_string(): {"Utc": utc, "Message": text}}});
        s.apply(msg(
            62,
            "RaceControlMessages",
            rcm(
                0,
                "2026-10-25T20:01:00",
                "FIA STEWARDS: 5 SECOND TIME PENALTY FOR CAR 18 (STR) - TRACK LIMITS",
            ),
        ));
        s.apply(msg(
            122,
            "RaceControlMessages",
            rcm(
                1,
                "2026-10-25T20:02:00",
                "FIA STEWARDS: PENALTY SERVED - 5 SECOND TIME PENALTY FOR CAR 18 (STR) - TRACK LIMITS",
            ),
        ));
        assert_eq!(
            s.penalised(),
            &HashMap::from([("18".to_owned(), parse_utc("2026-10-25T20:01:02Z").unwrap())])
        );
    }

    #[test]
    fn in_lap_is_not_the_best_lap() {
        let mut s = State::default();
        s.apply(msg(
            0,
            "SessionInfo",
            json!({"Type": "Qualifying", "Name": "Qualifying"}),
        ));
        let sector =
            |v: &str, g: u32| json!({"Value": v, "Segments": [{"Status": g}, {"Status": g}]});
        // Best lap just over the line: the live lap is it.
        let ch = s.apply(msg(
            1,
            "TimingData",
            json!({"SessionPart": 1, "Lines": {"27": {
                "Position": "10", "LastLapTime": {"Value": "1:33.165"},
                "BestLapTimes": [{"Value": "1:33.165"}],
                "Sectors": [sector("27.743", 2049), sector("39.222", 2049), sector("26.200", 2049)],
            }}}),
        ));
        s.derive(ch, laps::TrendMode::Avg3);
        let best = |s: &State| -> Vec<String> {
            s.views.tower[0]
                .quali
                .as_ref()
                .unwrap()
                .sectors
                .iter()
                .map(|x| x.value.clone())
                .collect()
        };
        assert_eq!(best(&s), ["27.743", "39.222", "26.200"]);

        // An in-lap, whole and with the same last lap time, without having
        // seen the reset that copies the best lap (Singapore 2026, HUL).
        let mut s = State::default();
        s.apply(msg(
            0,
            "SessionInfo",
            json!({"Type": "Qualifying", "Name": "Qualifying"}),
        ));
        let ch = s.apply(msg(
            1,
            "TimingData",
            json!({"SessionPart": 1, "Lines": {"27": {
                "Position": "10", "LastLapTime": {"Value": "1:33.165"},
                "BestLapTimes": [{"Value": "1:33.165"}],
                "Sectors": [sector("33.136", 2048), sector("47.121", 2048), sector("", 2064)],
            }}}),
        ));
        s.derive(ch, laps::TrendMode::Avg3);
        assert_eq!(best(&s), ["", "", ""]);
    }

    #[test]
    fn stopped_car_rules() {
        let mut s = State::default();
        s.apply(msg(0, "Heartbeat", json!({"Utc": "2025-03-16T04:00:00Z"})));
        s.apply(msg(1, "SessionStatus", json!({"Status": "Started"})));
        s.apply(msg(1, "TrackStatus", json!({"Status": "1"})));
        s.apply(msg(
            2,
            "TimingData",
            json!({"Lines": {"7": {"Position": "5", "InPit": false, "PitOut": false, "Stopped": false, "NumberOfLaps": 0}}}),
        ));
        assert!(s.stops.stopped.is_empty());

        let ch = s.apply(msg(
            30,
            "TimingData",
            json!({"Lines": {"7": {"Stopped": true}}}),
        ));
        assert!(ch.stops);
        let info = &s.stops.stopped["7"];
        assert_eq!(info.since.to_rfc3339(), "2025-03-16T04:00:30+00:00");
        assert_eq!(info.lap, 1);

        // Red flag freezes the list: nothing added, nothing removed.
        s.apply(msg(40, "TrackStatus", json!({"Status": "5"})));
        s.apply(msg(
            41,
            "TimingData",
            json!({"Lines": {"7": {"InPit": true}}}),
        ));
        assert!(s.stops.stopped.contains_key("7"));
        s.apply(msg(
            42,
            "TimingData",
            json!({"Lines": {"7": {"InPit": false}}}),
        ));
        s.apply(msg(50, "TrackStatus", json!({"Status": "1"})));
        assert_eq!(s.stops.stopped["7"].lap, 1);

        // Into the pit lane: no longer stopped on track.
        s.apply(msg(
            60,
            "TimingData",
            json!({"Lines": {"7": {"InPit": true}}}),
        ));
        assert!(s.stops.stopped.is_empty());
    }

    #[test]
    fn retirement_can_be_undone() {
        let mut s = State::default();
        let t = |d: Value| msg(0, "TimingData", json!({"Lines": {"11": d}}));
        s.apply(t(
            json!({"Retired": false, "InPit": true, "NumberOfLaps": 0}),
        ));
        s.apply(t(json!({"Retired": true, "Stopped": true})));
        assert!(s.retired.contains("11"));
        s.apply(t(
            json!({"Retired": false, "InPit": false, "Stopped": false}),
        ));
        assert!(!s.retired.contains("11"));

        // Retired by a long stop, then completes a lap.
        s.retired.insert("11".into());
        s.apply(t(json!({"NumberOfLaps": 1})));
        assert!(!s.retired.contains("11"));
    }

    #[test]
    fn grid_is_not_stopping() {
        grid_run(0);
    }

    /// A replay seek skips old telemetry: the restart's messages arrive
    /// before any car does.
    #[test]
    fn grid_survives_missing_telemetry() {
        grid_run(20);
    }

    /// Telemetry starts `from` seconds into the formation lap.
    fn grid_run(from: i64) {
        let mut s = State::default();
        s.apply(msg(0, "Heartbeat", json!({"Utc": "2026-09-06T13:30:00Z"})));
        let t0 = s.now();
        let lines: serde_json::Map<String, Value> = (1..=8)
            .map(|n| {
                (
                    n.to_string(),
                    json!({"InPit": false, "PitOut": false, "NumberOfLaps": 4}),
                )
            })
            .collect();
        s.apply(msg(0, "TimingData", json!({ "Lines": lines })));
        s.apply(msg(0, "TrackStatus", json!({"Status": "1"})));
        if from == 0 {
            for car in 1..=8 {
                s.cars.push_speed(&car.to_string(), t0, 100.0);
            }
        }
        // A resumption: Started comes with the formation lap.
        s.apply(msg(0, "SessionStatus", json!({"Status": "Started"})));
        assert!(matches!(s.grid, StartPhase::AwaitGrid(_)));

        // Car n pulls into its grid slot at 10 + 2n s; lights out at 60 s;
        // car 3 then stalls for good.
        let speed = |car: i64, sec: i64| match sec {
            _ if car == 3 && sec >= 61 => 0.0,
            60.. => 100.0,
            _ if sec >= 10 + 2 * car => 0.0,
            _ => 100.0,
        };
        let mut stopped_at = None;
        for sec in 0..80 {
            let now = t0 + TimeDelta::seconds(sec);
            s.feed_utc = Some(now);
            for car in (1..=8).filter(|_| sec >= from) {
                s.cars.push_speed(&car.to_string(), now, speed(car, sec));
            }
            s.eval_all_stops();
            if sec < 60 {
                assert!(s.stops.stopped.is_empty(), "{sec}s: {:?}", s.stops.stopped);
            }
            if sec == 50 {
                assert!(matches!(s.grid, StartPhase::OnGrid(_)));
            }
            if stopped_at.is_none() && s.stops.stopped.contains_key("3") {
                stopped_at = Some(sec);
            }
        }
        assert_eq!(s.grid, StartPhase::Racing);
        // Still for 5 s after lights out, and only car 3.
        assert_eq!(stopped_at, Some(66));
        assert_eq!(s.stops.stopped.len(), 1);
    }

    #[test]
    fn current_stint_follows_feed_updates() {
        // Belgium 2025: F1 moved PIA's current stint back to index 1 and
        // left a stale MEDIUM at index 2.
        let mut s = State::default();
        let app = |d: Value| msg(0, "TimingAppData", json!({"Lines": {"81": {"Stints": d}}}));
        s.apply(app(
            json!([{"Compound": "INTERMEDIATE", "TotalLaps": 0, "New": "true"}]),
        ));
        s.apply(app(
            json!({"1": {"Compound": "INTERMEDIATE", "TotalLaps": 0, "New": "true"}}),
        ));
        s.apply(app(
            json!({"2": {"Compound": "MEDIUM", "TotalLaps": 0, "New": "true"}}),
        ));
        s.apply(app(json!({"2": {"TotalLaps": 3}})));
        assert_eq!(s.active_stint("81"), Some(2));
        s.apply(app(json!({"1": {"Compound": "MEDIUM", "TotalLaps": 3}})));
        s.apply(app(json!({"1": {"TotalLaps": 12}})));
        s.apply(app(json!({"2": {"LapTime": "1:46.300", "LapNumber": 22}})));
        assert_eq!(s.active_stint("81"), Some(1));
        s.apply(msg(
            0,
            "TimingData",
            json!({"Lines": {"81": {"Position": "1"}}}),
        ));
        s.derive(Changed::all(), laps::TrendMode::Avg3);
        let t = s.views.tower[0].tyre.as_ref().unwrap();
        assert_eq!(
            (t.compound, t.age, t.stints),
            (parse::Compound::Medium, 12, 2)
        );
    }

    #[test]
    fn segment_updates_skip_the_tower() {
        assert!(!touches_tower(
            &json!({"Lines": {"27": {"Sectors": {"2": {"Segments": {"3": {"Status": 2048}}}}}}})
        ));
        assert!(!touches_tower(
            &json!({"Lines": {"4": {"Speeds": {"I1": {"Value": "282"}}}}})
        ));
        assert!(touches_tower(
            &json!({"Lines": {"4": {"Sectors": {}, "NumberOfLaps": 3}}})
        ));
        assert!(touches_tower(&json!({"SessionPart": 2})));
    }

    #[test]
    fn snapshot_replaces_state() {
        let mut s = State::default();
        s.apply(msg(
            0,
            "LapCount",
            json!({"CurrentLap": 3, "TotalLaps": 50}),
        ));
        let mut snap = Map::new();
        snap.insert("TrackStatus".into(), json!({"Status": "4", "_kf": true}));
        s.apply_snapshot(snap);
        assert!(s.topics.get("LapCount").is_none());
        assert_eq!(s.topics["TrackStatus"], json!({"Status": "4"}));
        assert_eq!(s.track_state(), TrackState::SafetyCar);
    }
}
