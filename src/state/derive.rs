//! Typed views built from the merged topics. Only the views affected by a
//! batch of messages are rebuilt.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, NaiveDateTime, TimeDelta, Utc};
use serde_json::Value;

use super::State;
use super::laps::{Trend, TrendMode};
use super::merge::indexed;
use super::parse::parse_laptime;
use super::parse::{Compound, Gap, as_bool, as_f64, as_str, as_u32, parse_colour, parse_gap};
use super::quali::{Phase, SectorSnap, adds_up, lap_sectors, part_best, project};
use super::title::{self, Entry, Fight, Rules};

#[derive(Debug, Default)]
pub struct Views {
    pub session: SessionView,
    pub track: TrackState,
    pub sector_flags: BTreeMap<u32, SectorFlag>,
    /// Highest marshal sector number race control has mentioned, for when
    /// circuit data doesn't say how many there are.
    pub max_sector_seen: u32,
    /// Newest first.
    pub rcm: Vec<Rcm>,
    pub tower: Vec<TowerRow>,
    /// Most recent first.
    pub stopped: Vec<StoppedCar>,
    /// Flags shown to a single driver, by racing number.
    pub driver_flags: HashMap<String, DriverFlag>,
    /// Penalties issued and not yet served, by racing number, oldest first.
    pub penalties: HashMap<String, Vec<Penalty>>,
    pub weather: Option<Weather>,
    /// In qualifying parts 1 and 2, how many cars go through to the next.
    pub advance: Option<usize>,
    /// Qualifying cars on a push lap, or just off one, earliest start first.
    pub pushing: Vec<PushRow>,
    /// Races: what each driver who could secure the title here needs.
    pub title: Vec<title::Block>,
}

/// A qualifying push lap, in progress or just finished.
#[derive(Debug, Clone)]
pub struct PushRow {
    pub num: String,
    pub tla: String,
    pub colour: Option<(u8, u8, u8)>,
    pub pos: u32,
    pub start: DateTime<Utc>,
    /// When it crossed the line, with the lap time.
    pub end: Option<(DateTime<Utc>, String, TimeKind)>,
    pub sectors: Vec<SectorSnap>,
    /// Ahead (negative) or behind the time that matters to this car, over
    /// the sectors done so far.
    pub benchmark: Option<(Benchmark, f64)>,
    /// Where the lap would put the car, projected from its own best lap.
    pub pred: Option<u32>,
}

/// The time a car on a push lap is up against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Benchmark {
    /// The fastest this part.
    P1,
    /// The last car through, for a car in the drop zone.
    Cut,
}

/// The latest weather sample, with changes since the one before.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Weather {
    /// °C.
    pub air: Reading,
    /// °C.
    pub track: Reading,
    /// %.
    pub humidity: Reading,
    /// m/s.
    pub wind: Reading,
    /// Degrees the wind blows from.
    pub wind_dir: Option<f64>,
    pub rain: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub value: f64,
    /// Change since the previous sample; `None` before there is one.
    pub delta: Option<f64>,
}

impl Weather {
    /// `None` until the feed has sent a sample.
    fn parse(cur: &Value, prev: &Value) -> Option<Self> {
        let reading = |k: &str| {
            as_f64(&cur[k]).map(|value| Reading {
                value,
                delta: as_f64(&prev[k]).map(|p| value - p),
            })
        };
        Some(Self {
            air: reading("AirTemp")?,
            track: reading("TrackTemp")?,
            humidity: reading("Humidity")?,
            wind: reading("WindSpeed")?,
            wind_dir: as_f64(&cur["WindDirection"]),
            rain: as_bool(&cur["Rainfall"]) == Some(true),
        })
    }
}

/// Compass point for a bearing in degrees: `N`, `NNE`, … `NNW`.
pub fn compass(deg: f64) -> &'static str {
    const POINTS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    POINTS[((deg.rem_euclid(360.0) / 22.5).round() as usize) % 16]
}

/// A penalty from the stewards, to be served in the pit lane or added to
/// the race time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Penalty {
    /// Seconds added at the next stop, or to the race time.
    Time(u32),
    DriveThrough,
    StopGo,
}

/// Flags race control shows to one driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DriverFlag {
    /// Black and white: a warning for unsporting behaviour or track limits.
    /// Stands for the rest of the session.
    BlackWhite,
    /// Black and orange: a mechanical problem, must pit. Withdrawn once the
    /// car enters the pit lane.
    BlackOrange,
    /// Black: disqualified.
    Black,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Race,
    Qualifying,
    Practice,
    #[default]
    Unknown,
}

impl SessionKind {
    /// Sessions run for the fastest lap, shown as standings and push laps.
    pub fn timed(self) -> bool {
        matches!(self, SessionKind::Qualifying | SessionKind::Practice)
    }
}

#[derive(Debug, Default)]
pub struct SessionView {
    pub title: String,
    pub kind: SessionKind,
    pub status: String,
    pub lap: Option<u32>,
    pub total_laps: Option<u32>,
    pub clock: Option<Clock>,
    pub circuit_key: Option<i64>,
    pub year: Option<i32>,
    /// Qualifying part (1-3).
    pub part: Option<u32>,
    /// A race's time limit, which its clock counts down from: an hour for a
    /// sprint, two for a grand prix.
    pub race_length: Option<TimeDelta>,
    pub sprint: bool,
}

impl SessionView {
    /// Race time: how far the race clock has run since lights out. Like the
    /// clock, it stops under a red flag.
    pub fn race_time_at(&self, now: DateTime<Utc>) -> Option<TimeDelta> {
        let (len, clock) = (self.race_length?, self.clock?);
        let started = !matches!(self.status.as_str(), "" | "Inactive");
        started.then(|| (len - clock.remaining_at(now)).max(TimeDelta::zero()))
    }

    /// When the session clock reads `remaining`, or for a race, when the
    /// race time reads `race_time`. Only while the clock runs.
    pub fn when_clock_reads(&self, shown: TimeDelta) -> Option<DateTime<Utc>> {
        let clock = self.clock.filter(|c| c.extrapolating)?;
        let remaining = match self.race_length {
            Some(len) => len - shown,
            None => shown,
        };
        Some(clock.utc + (clock.remaining - remaining))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Clock {
    pub utc: DateTime<Utc>,
    pub remaining: TimeDelta,
    pub extrapolating: bool,
}

impl Clock {
    pub fn remaining_at(&self, now: DateTime<Utc>) -> TimeDelta {
        if !self.extrapolating {
            return self.remaining;
        }
        (self.remaining - (now - self.utc)).max(TimeDelta::zero())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum TrackState {
    #[default]
    Unknown,
    Clear,
    Yellow,
    SafetyCar,
    /// SC still out, but race control has said it comes in this lap.
    SafetyCarEnding,
    Red,
    Vsc,
    VscEnding,
}

impl TrackState {
    pub fn parse(code: &str) -> Self {
        match code {
            "1" => Self::Clear,
            "2" => Self::Yellow,
            "4" => Self::SafetyCar,
            "5" => Self::Red,
            "6" => Self::Vsc,
            "7" => Self::VscEnding,
            _ => Self::Unknown,
        }
    }

    /// SC, VSC or red: lap times mean nothing.
    pub fn neutralised(&self) -> bool {
        matches!(
            self,
            Self::SafetyCar | Self::SafetyCarEnding | Self::Red | Self::Vsc | Self::VscEnding
        )
    }

    /// Statuses that flash while they last.
    pub fn flashes(&self) -> bool {
        matches!(self, Self::SafetyCar | Self::Vsc)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectorFlag {
    Yellow,
    DoubleYellow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcmKind {
    Green,
    Yellow,
    Red,
    Chequered,
    SafetyCar,
    Penalty,
    Drs,
    Blue,
    Other,
}

#[derive(Debug, Clone)]
pub struct Rcm {
    pub utc: Option<DateTime<Utc>>,
    pub lap: Option<u32>,
    pub text: String,
    pub kind: RcmKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeKind {
    Overall,
    Personal,
    Normal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Pit,
    Ret,
    /// Took the chequered flag.
    Fin,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tyre {
    pub compound: Compound,
    pub age: u32,
    pub new: bool,
    pub stints: usize,
}

#[derive(Debug, Clone)]
pub struct TowerRow {
    pub num: String,
    pub tla: String,
    pub colour: Option<(u8, u8, u8)>,
    pub pos: u32,
    pub interval: String,
    /// Within one second of the car ahead in a race.
    pub drs: bool,
    pub gap: String,
    pub laps: u32,
    pub last: String,
    pub last_kind: TimeKind,
    pub best: String,
    pub best_kind: TimeKind,
    pub pits: u32,
    pub tag: Option<Tag>,
    pub tyre: Option<Tyre>,
    pub trend: Option<Trend>,
    /// Own pace in seconds per lap, positive when getting faster.
    pub pace: Option<f64>,
    /// Knocked out of qualifying.
    pub out: bool,
    /// Qualifying only.
    pub quali: Option<QualiInfo>,
}

/// Where a car is in its qualifying run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Pit,
    Out,
    Push,
    Cool,
    /// Took the chequered flag.
    Fin,
}

#[derive(Debug, Clone)]
pub struct QualiInfo {
    /// Best time and gap to the fastest in the current part.
    pub best: String,
    pub gap: String,
    pub run: RunState,
    /// When the current lap (or out lap) started.
    pub lap_start: Option<DateTime<Utc>>,
    /// The sectors of the best lap this part.
    pub sectors: Vec<SectorSnap>,
}

#[derive(Debug, Clone)]
pub struct StoppedCar {
    pub num: String,
    pub tla: String,
    pub lap: u32,
    pub since: DateTime<Utc>,
}

/// Which views a batch of messages touched.
#[derive(Debug, Default, Clone, Copy)]
pub struct Changed {
    pub session: bool,
    pub track: bool,
    pub rcm: bool,
    pub tower: bool,
    pub stops: bool,
    pub map: bool,
    pub weather: bool,
}

impl Changed {
    pub fn all() -> Self {
        Self {
            session: true,
            track: true,
            rcm: true,
            tower: true,
            stops: true,
            map: true,
            weather: true,
        }
    }

    pub fn any_text(&self) -> bool {
        self.session || self.track || self.rcm || self.tower || self.stops || self.weather
    }

    pub fn union(&mut self, o: Changed) {
        self.session |= o.session;
        self.track |= o.track;
        self.rcm |= o.rcm;
        self.tower |= o.tower;
        self.stops |= o.stops;
        self.map |= o.map;
        self.weather |= o.weather;
    }
}

impl State {
    pub fn derive(&mut self, ch: Changed, mode: TrendMode) {
        if ch.session {
            self.views.session = session_view(&self.topics);
        }
        if ch.track || ch.rcm {
            let mut track = TrackState::parse(as_str(&self.topics_value("TrackStatus")["Status"]));
            // TrackStatus stays "SC deployed" until the restart; the ending
            // is announced by race control as "SAFETY CAR IN THIS LAP".
            // VSC ending has its own code (7), which arrives with race
            // control's "VIRTUAL SAFETY CAR ENDING"; either is enough.
            let rcm = self.topics_value("RaceControlMessages");
            track = match track {
                TrackState::SafetyCar if sc_status(rcm, SC) == Some("IN THIS LAP") => {
                    TrackState::SafetyCarEnding
                }
                TrackState::Vsc
                    if [VSC, VSC_SHORT]
                        .iter()
                        .any(|&m| sc_status(rcm, m) == Some("ENDING")) =>
                {
                    TrackState::VscEnding
                }
                t => t,
            };
            self.views.track = track;
        }
        if ch.rcm {
            let msgs = indexed(
                &self
                    .topics
                    .get("RaceControlMessages")
                    .unwrap_or(&Value::Null)["Messages"],
            );
            self.views.sector_flags = sector_flags(&msgs);
            self.views.max_sector_seen = msgs
                .iter()
                .filter_map(|m| as_u32(&m["Sector"]))
                .max()
                .unwrap_or(0);
            self.views.rcm = msgs.iter().rev().map(|m| rcm(m)).collect();
        }
        if ch.rcm || ch.tower {
            let msgs = indexed(
                &self
                    .topics
                    .get("RaceControlMessages")
                    .unwrap_or(&Value::Null)["Messages"],
            );
            self.views.driver_flags = driver_flags(&msgs, &self.pit_entry);
            self.views.penalties = penalties(&msgs);
        }
        if ch.tower || ch.stops || ch.session {
            self.views.tower = self.tower(mode);
            let timing = self.topics_value("TimingData");
            self.views.advance = (self.views.session.kind == SessionKind::Qualifying)
                .then(|| {
                    let p = as_u32(&timing["SessionPart"])?;
                    as_u32(indexed(&timing["NoEntries"]).get(p as usize)?)
                })
                .flatten()
                .map(|n| n as usize);
            self.views.pushing = self.pushing();
            self.views.title = self.title();
            if self.views.session.kind.timed() {
                let rows: Vec<(String, u32, String)> = self
                    .views
                    .tower
                    .iter()
                    .filter(|r| !r.out)
                    .filter_map(|r| Some((r.num.clone(), r.pos, r.quali.as_ref()?.best.clone())))
                    .collect();
                let now = self.now();
                self.quali.on_standings(&rows, now);
            }
        }
        if ch.stops || ch.tower {
            self.views.stopped = self.stopped_view();
        }
        if ch.weather {
            self.views.weather =
                Weather::parse(self.topics_value("WeatherData"), &self.weather_prev);
        }
    }

    fn tower(&self, mode: TrendMode) -> Vec<TowerRow> {
        let timing = self.topics_value("TimingData");
        let Some(lines) = timing["Lines"].as_object() else {
            return Vec::new();
        };
        let drivers = self.topics_value("DriverList");
        let app = &self.topics_value("TimingAppData")["Lines"];
        let kind = self.views.session.kind;
        let part = as_u32(&timing["SessionPart"]).or(self.views.session.part);

        let mut by_pos: Vec<(&String, &Value)> = lines.iter().collect();
        let pos_of = |l: &Value| {
            as_u32(&l["Position"])
                .or_else(|| as_u32(&l["Line"]))
                .unwrap_or(99)
        };
        by_pos.sort_by_key(|(_, l)| pos_of(l));
        let num_at: BTreeMap<u32, &String> = by_pos.iter().map(|(n, l)| (pos_of(l), *n)).collect();

        let mut rows: Vec<TowerRow> = by_pos
            .iter()
            .map(|&(num, l)| {
                let d = &drivers[num];
                let pos = pos_of(l);
                let ahead = pos
                    .checked_sub(1)
                    .and_then(|p| num_at.get(&p))
                    .map(|n| (*n).clone());

                // In qualifying, a knocked-out driver's figures are from the
                // last part they set a time in.
                let quali_part = part.filter(|_| kind == SessionKind::Qualifying).map(|p| {
                    let times = indexed(&l["BestLapTimes"]);
                    (1..=p)
                        .rev()
                        .find(|&i| {
                            times
                                .get(i as usize - 1)
                                .is_some_and(|t| !as_str(&t["Value"]).is_empty())
                        })
                        .unwrap_or(p)
                });
                let (interval, gap) = match (kind, quali_part) {
                    (SessionKind::Race, _) => (
                        as_str(&l["IntervalToPositionAhead"]["Value"]).to_owned(),
                        as_str(&l["GapToLeader"]).to_owned(),
                    ),
                    (SessionKind::Qualifying, Some(p)) if p >= 1 => {
                        let st = &l["Stats"][p as usize - 1];
                        (
                            as_str(&st["TimeDifftoPositionAhead"]).to_owned(),
                            as_str(&st["TimeDiffToFastest"]).to_owned(),
                        )
                    }
                    _ => (
                        as_str(&l["TimeDiffToPositionAhead"]).to_owned(),
                        as_str(&l["TimeDiffToFastest"]).to_owned(),
                    ),
                };
                let interval_gap = parse_gap(&interval);
                let interval = match interval_gap {
                    Gap::Leader if kind == SessionKind::Race => "Leader".to_owned(),
                    _ => interval,
                };
                let drs =
                    kind == SessionKind::Race && matches!(interval_gap, Gap::Seconds(s) if s < 1.0);

                let (best, best_kind) = match quali_part {
                    Some(p) if p >= 1 => {
                        let t = &indexed(&l["BestLapTimes"]);
                        let t = t.get(p as usize - 1).map_or("", |t| as_str(&t["Value"]));
                        (t.to_owned(), TimeKind::Normal)
                    }
                    _ => (
                        as_str(&l["BestLapTime"]["Value"]).to_owned(),
                        TimeKind::Normal,
                    ),
                };

                let last = &l["LastLapTime"];
                let last_kind = if as_bool(&last["OverallFastest"]) == Some(true) {
                    TimeKind::Overall
                } else if as_bool(&last["PersonalFastest"]) == Some(true) {
                    TimeKind::Personal
                } else {
                    TimeKind::Normal
                };

                let flag = |k: &str| as_bool(&l[k]) == Some(true);
                let tag = if flag("Retired") || self.retired.contains(num) {
                    Some(Tag::Ret)
                } else if as_u32(&l["Status"]).is_some_and(|s| s & STATUS_FINISHED != 0) {
                    Some(Tag::Fin)
                } else if flag("InPit") {
                    Some(Tag::Pit)
                } else {
                    None
                };

                let stints = indexed(&app[num]["Stints"]);
                let cur = self
                    .active_stint(num)
                    .filter(|&i| i < stints.len())
                    .or(stints.len().checked_sub(1));
                let tyre = cur.map(|i| (i, stints[i])).map(|(i, s)| Tyre {
                    compound: Compound::parse(as_str(&s["Compound"])),
                    age: as_u32(&s["TotalLaps"]).unwrap_or(0),
                    new: as_bool(&s["New"]).unwrap_or(true),
                    stints: i + 1,
                });

                let trend = (kind == SessionKind::Race)
                    .then(|| self.laps.trend(num, ahead.as_deref(), interval_gap, mode))
                    .flatten();
                let pace = (kind == SessionKind::Race)
                    .then(|| self.laps.pace(num, mode))
                    .flatten();

                TowerRow {
                    num: num.clone(),
                    tla: tla(d, num),
                    colour: parse_colour(as_str(&d["TeamColour"])),
                    pos,
                    interval,
                    drs,
                    gap,
                    laps: as_u32(&l["NumberOfLaps"]).unwrap_or(0),
                    last: as_str(&last["Value"]).to_owned(),
                    last_kind,
                    best,
                    best_kind,
                    // NumberOfPitStops also counts drive-throughs, such as the
                    // whole field following the SC through the pit lane. Those
                    // open a stint marked TyresNotChanged; count tyre changes.
                    pits: cur.map_or_else(
                        || as_u32(&l["NumberOfPitStops"]).unwrap_or(0),
                        |i| {
                            stints[1..=i]
                                .iter()
                                .filter(|s| as_bool(&s["TyresNotChanged"]) != Some(true))
                                .count() as u32
                        },
                    ),
                    tag,
                    tyre,
                    trend,
                    pace,
                    out: flag("KnockedOut"),
                    quali: kind.timed().then(|| self.quali_info(num, l, part)),
                }
            })
            .collect();
        mark_best(&mut rows);
        rows
    }

    /// The title fight in a race, from the points before it (F1's feed) and
    /// the running order. Tracks who could secure it and who would now.
    fn title(&mut self) -> Vec<title::Block> {
        let s = &self.views.session;
        let season = self.season.as_ref().filter(|se| Some(se.year) == s.year);
        let points = &self.topics_value("ChampionshipPrediction")["Drivers"];
        let (Some(season), true) = (season, s.kind == SessionKind::Race) else {
            return Vec::new();
        };
        if points.as_object().is_none_or(|p| p.is_empty()) {
            return Vec::new();
        }
        let entries = self
            .views
            .tower
            .iter()
            .map(|r| Entry {
                num: r.num.clone(),
                tla: r.tla.clone(),
                points: as_f64(&points[&r.num]["CurrentPoints"]).unwrap_or(0.0),
                wins: season.wins.get(&r.tla).copied().unwrap_or(0),
                pos: (r.tag != Some(Tag::Ret)).then_some(r.pos),
            })
            .collect();
        let fight = Fight::new(
            entries,
            &Rules::for_year(season.year),
            s.sprint,
            &season.later,
        );
        let mut blocks = fight.blocks();
        for b in &blocks {
            if !self.title_contenders.contains(&b.num) {
                self.title_contenders.push(b.num.clone());
            }
        }
        for num in &self.title_contenders {
            if !blocks.iter().any(|b| &b.num == num) {
                blocks.extend(fight.no_longer(num));
            }
        }
        let now = self.now();
        self.title_secured = match blocks.iter().find(|b| b.secured) {
            Some(b) => match self.title_secured.take() {
                Some((n, since)) if n == b.num => Some((n, since)),
                _ => Some((b.num.clone(), now)),
            },
            None => None,
        };
        blocks
    }

    fn quali_info(&self, num: &str, l: &Value, part: Option<u32>) -> QualiInfo {
        // This part's figures only: at the start of Q2 nobody has a time.
        let best = part_best(l, part);
        let gap = match part {
            Some(p) if p >= 1 => as_str(
                &indexed(&l["Stats"])
                    .get(p as usize - 1)
                    .map_or(&Value::Null, |s| *s)["TimeDiffToFastest"],
            ),
            // Practice.
            _ => as_str(&l["TimeDiffToFastest"]),
        };
        let phase = self.quali.phase(num);
        let run = if as_u32(&l["Status"]).is_some_and(|s| s & STATUS_FINISHED != 0) {
            RunState::Fin
        } else if as_bool(&l["InPit"]) == Some(true) {
            RunState::Pit
        } else {
            match phase {
                Phase::Out => RunState::Out,
                Phase::Push => RunState::Push,
                Phase::Cool => RunState::Cool,
            }
        };
        let live = lap_sectors(l);
        let mut sectors = match self.quali.best_lap(num, best) {
            Some(snap) => snap.to_vec(),
            // Just over the line, the lap that set the best time is still
            // the live one until its mini-sectors reset. An in-lap keeps the
            // last lap time too, so the sectors must add up to it.
            None if adds_up(&live, best) => live,
            None => live
                .into_iter()
                .map(|s| SectorSnap {
                    value: String::new(),
                    kind: TimeKind::Normal,
                    segments: vec![0; s.segments.len()],
                })
                .collect(),
        };
        self.quali.mark_fastest(&mut sectors);
        QualiInfo {
            best: best.to_owned(),
            gap: gap.to_owned(),
            run,
            lap_start: self.quali.lap_start(num),
            sectors,
        }
    }

    /// Cars on a push lap and the last push lap of each car, for the UI to
    /// keep a few seconds after the line.
    fn pushing(&self) -> Vec<PushRow> {
        let lines = &self.topics_value("TimingData")["Lines"];
        let in_part: Vec<(&TowerRow, &QualiInfo)> = self
            .views
            .tower
            .iter()
            .filter(|r| !r.out)
            .filter_map(|r| Some((r, r.quali.as_ref()?)))
            .collect();
        // The times to beat, and the laps that set them.
        let target = |r: Option<&(&TowerRow, &QualiInfo)>| {
            let (_, q) = r.filter(|(_, q)| !q.best.is_empty())?;
            Some((q.best.clone(), q.sectors.clone()))
        };
        let p1 = target(in_part.first());
        let cut = self
            .views
            .advance
            .and_then(|n| target(in_part.get(n.checked_sub(1)?)));

        let bests: Vec<(&str, Option<f64>)> = in_part
            .iter()
            .map(|(r, q)| (r.num.as_str(), parse_laptime(&q.best)))
            .collect();

        let mut rows: Vec<PushRow> = in_part
            .iter()
            .enumerate()
            .filter_map(|(i, (r, q))| {
                let l = &lines[&r.num];
                let (start, end, sectors) = if q.run == RunState::Push {
                    let mut live = lap_sectors(l);
                    if !self.quali.cleared(&r.num) {
                        // Still the lap before.
                        for s in &mut live {
                            s.value.clear();
                            s.segments.fill(0);
                        }
                    }
                    (q.lap_start?, None, live)
                } else {
                    let f = self.quali.finished(&r.num)?;
                    let last = &l["LastLapTime"];
                    let kind = if as_bool(&last["OverallFastest"]) == Some(true) {
                        TimeKind::Overall
                    } else if as_bool(&last["PersonalFastest"]) == Some(true) {
                        TimeKind::Personal
                    } else {
                        TimeKind::Normal
                    };
                    let time = as_str(&last["Value"]).to_owned();
                    let sectors = f.sectors.clone().unwrap_or_else(|| lap_sectors(l));
                    (f.start, Some((f.end, time, kind)), sectors)
                };
                let mut sectors = sectors;
                self.quali.mark_fastest(&mut sectors);
                // Without a time, or below the line, getting through is
                // what counts.
                let danger = q.best.is_empty() || self.views.advance.is_some_and(|n| i >= n);
                let (kind, target) = match &cut {
                    Some(c) if danger => (Benchmark::Cut, Some(c)),
                    _ => (Benchmark::P1, p1.as_ref()),
                };
                // Only while the lap is on: the standings have the result.
                let delta = target
                    .filter(|_| end.is_none())
                    .and_then(|(best, ref_sectors)| {
                        Some((lap_delta(&sectors, ref_sectors)?, parse_laptime(best)?))
                    });
                let benchmark = delta.map(|(d, _)| (kind, d));
                let pred = end
                    .is_none()
                    .then(|| {
                        let p1_sectors = p1.as_ref().map(|(_, s)| s.as_slice());
                        let projected = project(&sectors, &q.sectors, p1_sectors)?;
                        let mine = parse_laptime(&q.best).map_or(projected, |b| b.min(projected));
                        let ahead = bests
                            .iter()
                            .filter(|(n, b)| *n != r.num && b.is_some_and(|b| b < mine))
                            .count();
                        Some(ahead as u32 + 1)
                    })
                    .flatten();
                Some(PushRow {
                    num: r.num.clone(),
                    tla: r.tla.clone(),
                    colour: r.colour,
                    pos: r.pos,
                    start,
                    benchmark,
                    pred,
                    end,
                    sectors,
                })
            })
            .collect();
        rows.sort_by_key(|r| r.start);
        rows
    }

    fn stopped_view(&self) -> Vec<StoppedCar> {
        let drivers = self.topics_value("DriverList");
        let mut v: Vec<StoppedCar> = self
            .stops
            .stopped
            .iter()
            .filter(|(num, _)| !self.retired.contains(*num))
            .map(|(num, s)| StoppedCar {
                num: num.clone(),
                tla: tla(&drivers[num], num),
                lap: s.lap,
                since: s.since,
            })
            .collect();
        v.sort_by(|a, b| b.since.cmp(&a.since).then(a.num.cmp(&b.num)));
        v
    }
}

/// Marks the overall fastest best lap purple and the rest green.
fn mark_best(rows: &mut [TowerRow]) {
    let secs = |r: &TowerRow| super::parse::parse_laptime(&r.best);
    let fastest = rows.iter().filter_map(secs).fold(f64::INFINITY, f64::min);
    for r in rows.iter_mut() {
        r.best_kind = match secs(r) {
            Some(t) if (t - fastest).abs() < 1e-6 => TimeKind::Overall,
            Some(_) => TimeKind::Personal,
            None => TimeKind::Normal,
        };
    }
}

fn tla(driver: &Value, num: &str) -> String {
    match as_str(&driver["Tla"]) {
        "" => num.to_owned(),
        t => t.to_owned(),
    }
}

fn session_view(topics: &serde_json::Map<String, Value>) -> SessionView {
    let null = Value::Null;
    let get = |t: &str| topics.get(t).unwrap_or(&null);
    let info = get("SessionInfo");
    let meeting = as_str(&info["Meeting"]["Name"]);
    let name = as_str(&info["Name"]);
    let title = match (meeting, name) {
        ("", "") => String::new(),
        (m, "") => m.to_owned(),
        ("", n) => n.to_owned(),
        (m, n) => format!("{m} · {n}"),
    };
    let kind = match (as_str(&info["Type"]), name) {
        ("Race", _) => SessionKind::Race,
        ("Qualifying", _) => SessionKind::Qualifying,
        (_, n) if n.contains("Qualifying") || n.contains("Shootout") => SessionKind::Qualifying,
        ("Practice", _) => SessionKind::Practice,
        _ => SessionKind::Unknown,
    };
    let year = as_str(&info["StartDate"])
        .get(..4)
        .or_else(|| as_str(&info["Path"]).get(..4))
        .and_then(|y| y.parse().ok());
    let lc = get("LapCount");
    let clock = get("ExtrapolatedClock");
    let clock = parse_utc(as_str(&clock["Utc"])).and_then(|utc| {
        Some(Clock {
            utc,
            remaining: parse_hms(as_str(&clock["Remaining"]))?,
            extrapolating: as_bool(&clock["Extrapolating"]).unwrap_or(false),
        })
    });
    SessionView {
        title,
        kind,
        status: as_str(&get("SessionStatus")["Status"]).to_owned(),
        lap: as_u32(&lc["CurrentLap"]),
        total_laps: as_u32(&lc["TotalLaps"]),
        clock,
        circuit_key: info["Meeting"]["Circuit"]["Key"].as_i64(),
        year,
        part: as_u32(&get("TimingData")["SessionPart"]),
        race_length: (kind == SessionKind::Race)
            .then(|| TimeDelta::hours(if name.contains("Sprint") { 1 } else { 2 })),
        sprint: kind == SessionKind::Race && name.contains("Sprint"),
    }
}

pub fn parse_utc(s: &str) -> Option<DateTime<Utc>> {
    if s.is_empty() {
        return None;
    }
    s.parse::<DateTime<Utc>>().ok().or_else(|| {
        // Race control times have no zone and are UTC.
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
            .ok()
            .map(|n| n.and_utc())
    })
}

/// `"01:59:59"` → duration.
fn parse_hms(s: &str) -> Option<TimeDelta> {
    let mut secs = 0i64;
    for p in s.split(':') {
        secs = secs * 60 + p.trim().parse::<i64>().ok()?;
    }
    Some(TimeDelta::seconds(secs))
}

/// Whether the latest safety car message says it comes in this lap.
const SC: &str = "SAFETY CAR";
const VSC: &str = "VIRTUAL SAFETY CAR";
/// 2026 race control calls it "VSC".
const VSC_SHORT: &str = "VSC";

/// Status of race control's latest message for `mode` (SC or VSC), e.g.
/// "DEPLOYED", "IN THIS LAP" or "ENDING".
fn sc_status<'a>(rcm: &'a Value, mode: &str) -> Option<&'a str> {
    indexed(&rcm["Messages"])
        .iter()
        .rev()
        .find(|m| as_str(&m["Category"]) == "SafetyCar" && as_str(&m["Mode"]) == mode)
        .map(|m| as_str(&m["Status"]))
}

/// Bit of a TimingData line's `Status` set as the car crosses the line
/// after the chequered flag.
const STATUS_FINISHED: u32 = 1024;

/// How far `sectors` is behind (positive) a reference lap over the sectors
/// done so far.
fn lap_delta(sectors: &[SectorSnap], ref_sectors: &[SectorSnap]) -> Option<f64> {
    let mut delta = None;
    for (s, r) in sectors.iter().zip(ref_sectors) {
        let (Some(s), Some(r)) = (parse_laptime(&s.value), parse_laptime(&r.value)) else {
            break;
        };
        delta = Some(delta.unwrap_or(0.0) + s - r);
    }
    delta
}

/// Penalties still to be served, replaying messages in order. Only race
/// control's "PENALTY SERVED" clears one: a pass through the pit lane
/// may be a stop or just following the safety car through it.
fn penalties(msgs: &[&Value]) -> HashMap<String, Vec<Penalty>> {
    let mut out: HashMap<String, Vec<Penalty>> = HashMap::new();
    for m in msgs {
        let Some((served, p, num)) = parse_penalty(as_str(&m["Message"])) else {
            continue;
        };
        let pending = out.entry(num).or_default();
        if !served {
            pending.push(p);
        } else if let Some(i) = pending.iter().position(|x| *x == p) {
            pending.remove(i);
        } else if !pending.is_empty() {
            // Worded differently from the penalty: clear the oldest.
            pending.remove(0);
        }
    }
    out.retain(|_, v| !v.is_empty());
    out
}

/// Parses the stewards' penalty messages:
///
/// ```text
/// FIA STEWARDS: 5 SECOND TIME PENALTY FOR CAR 5 (BOR) - UNSAFE RELEASE
/// FIA STEWARDS: DRIVE THROUGH PENALTY FOR CAR 44 (HAM) - ...
/// FIA STEWARDS: 10 SECOND STOP/GO PENALTY FOR CAR 87 (BEA) - ...  (or STOP-AND-GO)
/// FIA STEWARDS: PENALTY SERVED - 5 SECOND TIME PENALTY FOR CAR 5 (BOR) - ...
/// ```
///
/// Returns (served, penalty, racing number). Grid penalties, reprimands
/// and warnings don't affect the race and are skipped.
pub fn parse_penalty(text: &str) -> Option<(bool, Penalty, String)> {
    let t = text.to_uppercase();
    let t = t.strip_prefix("FIA STEWARDS:")?.trim_start();
    let (served, t) = match t.strip_prefix("PENALTY SERVED -") {
        Some(rest) => (true, rest.trim_start()),
        None => (false, t),
    };
    let (head, tail) = t.split_once(" PENALTY FOR CAR ")?;
    let num: String = tail.chars().take_while(char::is_ascii_digit).collect();
    if num.is_empty() {
        return None;
    }
    let p = if head.contains("DRIVE THROUGH") {
        Penalty::DriveThrough
    } else if ["STOP/GO", "STOP-AND-GO", "STOP AND GO"]
        .iter()
        .any(|w| head.contains(w))
    {
        Penalty::StopGo
    } else {
        Penalty::Time(head.strip_suffix(" SECOND TIME")?.trim().parse().ok()?)
    };
    Some((served, p, num))
}

/// Each driver's most serious flag, replaying messages in order.
/// `pit_entry` is when each car last entered the pit lane.
fn driver_flags(
    msgs: &[&Value],
    pit_entry: &HashMap<String, DateTime<Utc>>,
) -> HashMap<String, DriverFlag> {
    let mut out: HashMap<String, DriverFlag> = HashMap::new();
    for m in msgs {
        let Some((flag, num)) = driver_flag(m) else {
            continue;
        };
        if flag == DriverFlag::BlackOrange {
            let pitted_since = match (parse_utc(as_str(&m["Utc"])), pit_entry.get(&num)) {
                (Some(t), Some(p)) => *p >= t,
                _ => false,
            };
            if pitted_since {
                continue;
            }
        }
        let e = out.entry(num).or_insert(flag);
        *e = (*e).max(flag);
    }
    out
}

/// A flag shown to one driver, and their racing number. Usually a `Flag`
/// message with `RacingNumber`, but now and then only the text says so:
/// `BLACK AND WHITE FLAG FOR CAR 30 (LAW) (15:53:37)` (Monza 2026).
fn driver_flag(m: &Value) -> Option<(DriverFlag, String)> {
    let parse = |f: &str| match f {
        "BLACK AND WHITE" => Some(DriverFlag::BlackWhite),
        "BLACK AND ORANGE" => Some(DriverFlag::BlackOrange),
        "BLACK" => Some(DriverFlag::Black),
        _ => None,
    };
    let num = as_str(&m["RacingNumber"]);
    if as_str(&m["Category"]) == "Flag" && !num.is_empty() {
        return Some((parse(as_str(&m["Flag"]))?, num.to_owned()));
    }
    let text = as_str(&m["Message"]).to_uppercase();
    let (flag, rest) = text.split_once(" FLAG FOR CAR ")?;
    let num: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (!num.is_empty()).then_some(())?;
    Some((parse(flag)?, num))
}

/// Rebuilds the marshal sector flag map by replaying messages in order.
fn sector_flags(msgs: &[&Value]) -> BTreeMap<u32, SectorFlag> {
    let mut map = BTreeMap::new();
    for m in msgs {
        if as_str(&m["Category"]) != "Flag" {
            continue;
        }
        let flag = as_str(&m["Flag"]);
        match (as_str(&m["Scope"]), flag) {
            (_, "RED" | "CHEQUERED") | ("Track", "GREEN" | "CLEAR") => map.clear(),
            ("Sector", f) => {
                let Some(s) = as_u32(&m["Sector"]) else {
                    continue;
                };
                match f {
                    "YELLOW" => {
                        map.insert(s, SectorFlag::Yellow);
                    }
                    "DOUBLE YELLOW" => {
                        map.insert(s, SectorFlag::DoubleYellow);
                    }
                    "CLEAR" | "GREEN" => {
                        map.remove(&s);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    map
}

fn rcm(m: &Value) -> Rcm {
    let text = as_str(&m["Message"]).to_owned();
    let upper = text.to_uppercase();
    let kind = match (as_str(&m["Category"]), as_str(&m["Flag"])) {
        ("Flag", "BLUE") => RcmKind::Blue,
        ("Flag", "YELLOW" | "DOUBLE YELLOW") => RcmKind::Yellow,
        ("Flag", "RED") => RcmKind::Red,
        ("Flag", "GREEN" | "CLEAR") => RcmKind::Green,
        ("Flag", "CHEQUERED") => RcmKind::Chequered,
        ("SafetyCar", _) => RcmKind::SafetyCar,
        ("Drs", _) => RcmKind::Drs,
        _ if [
            "STEWARDS",
            "PENALTY",
            "INVESTIGAT",
            "NOTED",
            "REVIEWED",
            "DELETED",
            "REPRIMAND",
        ]
        .iter()
        .any(|w| upper.contains(w)) =>
        {
            RcmKind::Penalty
        }
        _ if upper.contains("SAFETY CAR") => RcmKind::SafetyCar,
        _ => RcmKind::Other,
    };
    Rcm {
        utc: parse_utc(as_str(&m["Utc"])),
        lap: as_u32(&m["Lap"]),
        text,
        kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sector_flags_follow_messages() {
        // Sequence from the 2025 Australian GP.
        let msgs = [
            json!({"Category":"Flag","Flag":"YELLOW","Scope":"Sector","Sector":17}),
            json!({"Category":"Flag","Flag":"DOUBLE YELLOW","Scope":"Sector","Sector":4}),
            json!({"Category":"Flag","Flag":"CLEAR","Scope":"Sector","Sector":17}),
            json!({"Category":"Flag","Flag":"YELLOW","Scope":"Sector","Sector":5}),
            json!({"Category":"Flag","Flag":"BLUE","Scope":"Driver","RacingNumber":"18"}),
        ];
        let refs: Vec<&Value> = msgs.iter().collect();
        let f = sector_flags(&refs);
        assert_eq!(f.len(), 2);
        assert_eq!(f[&4], SectorFlag::DoubleYellow);
        assert_eq!(f[&5], SectorFlag::Yellow);

        let mut more = msgs.to_vec();
        more.push(json!({"Category":"Flag","Flag":"CLEAR","Scope":"Track"}));
        let refs: Vec<&Value> = more.iter().collect();
        assert!(sector_flags(&refs).is_empty());
    }

    #[test]
    fn safety_car_status_from_race_control() {
        let msg =
            |mode: &str, status: &str| json!({"Category":"SafetyCar","Status":status,"Mode":mode});
        let rcm = |msgs: Vec<Value>| json!({ "Messages": msgs });
        assert_eq!(
            sc_status(&rcm(vec![msg(SC, "DEPLOYED")]), SC),
            Some("DEPLOYED")
        );
        assert_eq!(
            sc_status(
                &rcm(vec![
                    msg(SC, "DEPLOYED"),
                    msg(SC, "IN THIS LAP"),
                    json!({"Category":"Flag"})
                ]),
                SC
            ),
            Some("IN THIS LAP")
        );
        // A new deployment after the last one ended.
        assert_eq!(
            sc_status(&rcm(vec![msg(SC, "IN THIS LAP"), msg(SC, "DEPLOYED")]), SC),
            Some("DEPLOYED")
        );
        // SC and VSC messages are kept apart.
        let both = rcm(vec![msg(SC, "IN THIS LAP"), msg(VSC, "ENDING")]);
        assert_eq!(sc_status(&both, SC), Some("IN THIS LAP"));
        assert_eq!(sc_status(&both, VSC), Some("ENDING"));
        assert_eq!(sc_status(&rcm(vec![]), VSC), None);
        let short = rcm(vec![msg(VSC_SHORT, "DEPLOYED"), msg(VSC_SHORT, "ENDING")]);
        assert_eq!(sc_status(&short, VSC_SHORT), Some("ENDING"));
    }

    #[test]
    fn penalty_messages() {
        let p = |t: &str| parse_penalty(t);
        assert_eq!(
            p("FIA STEWARDS: 5 SECOND TIME PENALTY FOR CAR 5 (BOR) - UNSAFE RELEASE"),
            Some((false, Penalty::Time(5), "5".into()))
        );
        assert_eq!(
            p(
                "FIA STEWARDS: PENALTY SERVED - 10 SECOND TIME PENALTY FOR CAR 44 (HAM) - CAUSING A COLLISION"
            ),
            Some((true, Penalty::Time(10), "44".into()))
        );
        assert_eq!(
            p("FIA STEWARDS: DRIVE THROUGH PENALTY FOR CAR 63 (RUS) - LEAVING THE TRACK"),
            Some((false, Penalty::DriveThrough, "63".into()))
        );
        assert_eq!(
            p("FIA STEWARDS: 10 SECOND STOP/GO PENALTY FOR CAR 87 (BEA) - UNSAFE CONDITION"),
            Some((false, Penalty::StopGo, "87".into()))
        );
        assert_eq!(
            p("FIA STEWARDS: PENALTY SERVED - STOP-AND-GO PENALTY FOR CAR 43 (COL) - X"),
            Some((true, Penalty::StopGo, "43".into()))
        );
        for no in [
            "FIA STEWARDS: INCIDENT INVOLVING CAR 14 (ALO) UNDER INVESTIGATION - SAFETY CAR INFRINGEMENT",
            "FIA STEWARDS: REPRIMAND (DRIVING) FOR CAR 55 (SAI) - ESCAPE ROAD INSTRUCTIONS",
            "FIA STEWARDS: 5 PLACE GRID PENALTY FOR CAR 1 (VER) - X",
            "CAR 5 (BOR) TIME 1:23.456 DELETED - TRACK LIMITS",
        ] {
            assert_eq!(p(no), None, "{no}");
        }
    }

    #[test]
    fn penalties_cleared_only_when_served() {
        let m = |t: &str| json!({ "Message": format!("FIA STEWARDS: {t}") });
        let msgs = [
            m("5 SECOND TIME PENALTY FOR CAR 18 (STR) - A"),
            m("5 SECOND TIME PENALTY FOR CAR 18 (STR) - B"),
            m("DRIVE THROUGH PENALTY FOR CAR 18 (STR) - C"),
            m("10 SECOND TIME PENALTY FOR CAR 44 (HAM) - D"),
            m("PENALTY SERVED - DRIVE THROUGH PENALTY FOR CAR 18 (STR) - C"),
            m("PENALTY SERVED - 5 SECOND TIME PENALTY FOR CAR 18 (STR) - A"),
            m("PENALTY SERVED - 10 SECOND TIME PENALTY FOR CAR 44 (HAM) - D"),
        ];
        let refs: Vec<&Value> = msgs.iter().collect();
        assert_eq!(
            penalties(&refs),
            HashMap::from([("18".to_owned(), vec![Penalty::Time(5)])])
        );
        assert_eq!(
            penalties(&refs[..4]).get("18"),
            Some(&vec![
                Penalty::Time(5),
                Penalty::Time(5),
                Penalty::DriveThrough
            ])
        );
    }

    #[test]
    fn driver_flags_from_race_control() {
        let flag = |utc: &str, num: &str, f: &str| json!({"Utc": utc, "Category":"Flag","Flag":f,"Scope":"Driver","RacingNumber":num});
        let msgs = [
            flag("2026-10-11T13:00:00", "12", "BLACK AND WHITE"),
            flag("2026-10-11T13:00:00", "18", "BLUE"),
            flag("2026-10-11T13:01:00", "44", "BLACK AND ORANGE"),
            flag("2026-10-11T13:02:00", "16", "BLACK AND ORANGE"),
            flag("2026-10-11T13:03:00", "12", "BLACK"),
            json!({"Category":"Flag","Flag":"BLACK AND WHITE","Scope":"Driver"}),
            json!({"Category":"Other","Message":"BLACK AND WHITE FLAG FOR CAR 30 (LAW) (15:53:37)"}),
            json!({"Category":"Other","Message":"WAVED BLUE FLAG FOR CAR 7 (DOO) TIMED AT 15:44:05"}),
        ];
        let refs: Vec<&Value> = msgs.iter().collect();
        // 44 pitted after its black and orange flag, 16 before.
        let pits = HashMap::from([
            ("44".to_owned(), parse_utc("2026-10-11T13:01:30").unwrap()),
            ("16".to_owned(), parse_utc("2026-10-11T13:01:30").unwrap()),
        ]);
        let f = driver_flags(&refs, &pits);
        assert_eq!(
            f,
            HashMap::from([
                ("12".to_owned(), DriverFlag::Black),
                ("16".to_owned(), DriverFlag::BlackOrange),
                ("30".to_owned(), DriverFlag::BlackWhite),
            ])
        );
        assert_eq!(
            driver_flags(&refs[..1], &pits).get("12"),
            Some(&DriverFlag::BlackWhite)
        );
    }

    #[test]
    fn rcm_kinds() {
        let k = |v: Value| rcm(&v).kind;
        assert_eq!(k(json!({"Category":"Flag","Flag":"BLUE"})), RcmKind::Blue);
        assert_eq!(
            k(json!({"Category":"SafetyCar","Mode":"VIRTUAL SAFETY CAR"})),
            RcmKind::SafetyCar
        );
        assert_eq!(
            k(
                json!({"Category":"Other","Message":"CAR 14 (ALO) TIME 1:23.456 DELETED - TRACK LIMITS"})
            ),
            RcmKind::Penalty
        );
        assert_eq!(
            k(json!({"Category":"Drs","Status":"ENABLED"})),
            RcmKind::Drs
        );
        assert_eq!(
            k(
                json!({"Category":"Other","Message":"FIA STEWARDS: INCIDENT INVOLVING CAR 14 (ALO) NO FURTHER ACTION - SAFETY CAR INFRINGEMENT"})
            ),
            RcmKind::Penalty
        );
        assert_eq!(
            k(json!({"Category":"Other","Message":"SAFETY CAR WILL USE START/FINISH STRAIGHT"})),
            RcmKind::SafetyCar
        );
        let r = rcm(&json!({"Utc":"2025-03-16T03:25:03","Lap":1,"Category":"Other","Message":"x"}));
        assert_eq!(r.utc.unwrap().to_rfc3339(), "2025-03-16T03:25:03+00:00");
    }

    #[test]
    fn race_time_and_video_sync() {
        let utc = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut s = SessionView {
            kind: SessionKind::Race,
            status: "Started".into(),
            race_length: Some(TimeDelta::hours(2)),
            clock: Some(Clock {
                utc,
                remaining: TimeDelta::seconds(7199),
                extrapolating: true,
            }),
            ..SessionView::default()
        };
        let now = utc + TimeDelta::seconds(303);
        assert_eq!(s.race_time_at(now), Some(TimeDelta::seconds(304)));
        // The video shows 5:04: that's `now`.
        assert_eq!(s.when_clock_reads(TimeDelta::seconds(304)), Some(now));

        // Qualifying: the time left.
        s.race_length = None;
        assert_eq!(s.race_time_at(now), None);
        assert_eq!(s.when_clock_reads(TimeDelta::seconds(6896)), Some(now));

        // A stopped clock can't be synced to.
        s.clock.as_mut().unwrap().extrapolating = false;
        assert_eq!(s.when_clock_reads(TimeDelta::seconds(6896)), None);
    }

    #[test]
    fn clock_extrapolates() {
        let c = Clock {
            utc: parse_utc("2025-03-16T04:18:23.004Z").unwrap(),
            remaining: TimeDelta::seconds(7199),
            extrapolating: true,
        };
        let now = c.utc + TimeDelta::seconds(60);
        assert_eq!(c.remaining_at(now).num_seconds(), 7139);
        assert_eq!(
            c.remaining_at(c.utc + TimeDelta::hours(3)),
            TimeDelta::zero()
        );
    }
}
