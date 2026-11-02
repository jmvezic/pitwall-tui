//! Benchmark of the qualifying position prediction against every archived
//! qualifying and sprint qualifying session of a season. Downloads into
//! `target/test-archive` on first run, so it is ignored by default:
//!
//! ```sh
//! cargo test --release quali_bench -- --ignored --nocapture
//! ```
//!
//! `BENCH_ONLY=Mexico` runs the sessions whose archive path contains it;
//! `BENCH_SESSIONS=1` prints a line per session.
//!
//! Each session is replayed as the app does, deriving the views every
//! quarter second. For every push lap that reaches the line, the predicted
//! position is checked against where the lap actually put the car: its
//! position five seconds after the line. Scored at the end of the first
//! sector (before its time), of the first and of the second, and over
//! every frame of the lap.
//!
//! What was known at the first and last frame after each sector, and the
//! result, go to `target/test-archive/quali_bench.jsonl`, one lap per
//! line, to try other predictions on without replaying.

use std::collections::HashMap;
use std::fmt::Write;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};

use super::State;
use super::derive::{PushRow, TowerRow, Views};
use super::laps::TrendMode;
use super::parse::parse_laptime;
use super::quali::SectorSnap;
use crate::feed::{self, archive};

const YEAR: i32 = 2026;
/// How often the views are derived, as the app does each frame.
const FRAME: Duration = Duration::from_millis(250);
/// The position this long after the line is the lap's result.
const SETTLE: TimeDelta = TimeDelta::seconds(5);

/// One push lap, from its start to its result.
#[derive(Debug, Default, Clone)]
struct Lap {
    session: String,
    part: u32,
    tla: String,
    /// The last prediction and projected time with 0, 1 and 2 sectors done.
    at: [Option<u32>; 3],
    proj: [Option<f64>; 3],
    /// At the last frame with 2 sectors done, the car's best and the
    /// others': where a perfect projection would have ranked the lap.
    best_at2: Option<f64>,
    others_at2: Vec<f64>,
    /// What was known at the first and the last frame with 1 and 2
    /// sectors done.
    first: [Option<Value>; 3],
    last: [Option<Value>; 3],
    /// Every prediction shown, one per frame.
    frames: Vec<Option<u32>>,
    sectors: Vec<Option<f64>>,
    end: Option<DateTime<Utc>>,
    /// The best time before the lap, then the lap's time.
    time: String,
    /// Whether the lap beat the car's best before it.
    improved: bool,
    result: Option<u32>,
}

#[derive(Default)]
struct Score {
    laps: usize,
    /// Correct with 0, 1 and 2 sectors done.
    at: [usize; 3],
    /// Off by at most one place with 2 sectors done.
    near: usize,
    /// A perfect projected time, ranked as the app ranks, with 2 done.
    perfect: usize,
    frames: usize,
    frames_ok: usize,
    /// Projected lap time errors with 1 and 2 sectors done, improving
    /// laps only.
    err: [Vec<f64>; 2],
}

impl Score {
    fn add(&mut self, l: &Lap) {
        let Some(truth) = l.result else { return };
        self.laps += 1;
        for k in 0..3 {
            self.at[k] += usize::from(l.at[k] == Some(truth));
        }
        self.near += usize::from(l.at[2].is_some_and(|p| p.abs_diff(truth) <= 1));
        self.frames += l.frames.len();
        self.frames_ok += l.frames.iter().filter(|&&p| p == Some(truth)).count();
        let Some(t) = parse_laptime(&l.time) else {
            return;
        };
        let mine = l.best_at2.map_or(t, |b| b.min(t));
        let perfect = 1 + l.others_at2.iter().filter(|&&b| b < mine).count() as u32;
        self.perfect += usize::from(l.at[2].is_some() && perfect == truth);
        if l.improved {
            for k in 0..2 {
                if let Some(p) = l.proj[k + 1] {
                    self.err[k].push((p - t).abs());
                }
            }
        }
    }

    fn line(&self, label: &str) -> String {
        let pc = |n: usize, d: usize| 100.0 * n as f64 / d.max(1) as f64;
        let median = |v: &[f64]| {
            let mut v = v.to_vec();
            v.sort_by(f64::total_cmp);
            v.get(v.len() / 2).copied().unwrap_or(f64::NAN)
        };
        format!(
            "{label:<26} laps {:>5}  in S1 {:>5.1}%  after S1 {:>5.1}%  after S2 {:>5.1}% \
             (±1 {:>5.1}%, perfect time {:>5.1}%)  all frames {:>5.1}%  \
             |time error| median after S1 {:.3}s, S2 {:.3}s",
            self.laps,
            pc(self.at[0], self.laps),
            pc(self.at[1], self.laps),
            pc(self.at[2], self.laps),
            pc(self.near, self.laps),
            pc(self.perfect, self.laps),
            pc(self.frames_ok, self.frames),
            median(&self.err[0]),
            median(&self.err[1]),
        )
    }
}

fn cache() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-archive")
}

fn token() -> Option<String> {
    std::env::var("PITWALL_TOKEN").ok().or_else(|| {
        crate::config::load(&crate::config::default_config_file())
            .ok()?
            .token
    })
}

/// Every qualifying and sprint qualifying session in the archive.
fn sessions(rt: &tokio::runtime::Runtime) -> Vec<String> {
    rt.block_on(async {
        let client = archive::http_client().unwrap();
        let index = archive::fetch_index(&client, token().as_deref(), YEAR, &cache())
            .await
            .unwrap();
        index
            .meetings
            .iter()
            .flat_map(|m| &m.sessions)
            .filter(|s| s.name.contains("Qualifying") || s.name.contains("Shootout"))
            .filter_map(|s| s.path.clone())
            .collect()
    })
}

fn times(sectors: &[SectorSnap]) -> Vec<Option<f64>> {
    sectors.iter().map(|s| parse_laptime(&s.value)).collect()
}

fn best(r: &TowerRow) -> Option<f64> {
    parse_laptime(&r.quali.as_ref()?.best)
}

/// What the app knew about lap `p` at this frame.
fn features(s: &State, v: &Views, p: &PushRow) -> Value {
    let row = v.tower.iter().find(|r| r.num == p.num);
    let q = row.and_then(|r| r.quali.as_ref());
    let done = p.sectors.iter().take_while(|x| !x.value.is_empty()).count();
    let p1 = v.tower.iter().filter(|r| !r.out).find_map(|r| {
        let q = r.quali.as_ref()?;
        (!q.best.is_empty()).then(|| times(&q.sectors))
    });
    let others: Vec<f64> = v
        .tower
        .iter()
        .filter(|r| !r.out && r.num != p.num)
        .filter_map(best)
        .collect();
    // Other laps on: number, start relative to this one (ms), sectors,
    // best time.
    let live: Vec<Value> = v
        .pushing
        .iter()
        .filter(|o| o.num != p.num && o.end.is_none())
        .map(|o| {
            let b = v.tower.iter().find(|r| r.num == o.num).and_then(best);
            json!([
                o.num,
                (o.start - p.start).num_milliseconds(),
                times(&o.sectors),
                b
            ])
        })
        .collect();
    let fastest: Vec<Option<f64>> = s
        .quali
        .session_fastest()
        .iter()
        .map(|t| t.is_finite().then_some(*t))
        .collect();
    json!({
        "done": times(&p.sectors),
        "segs": p.sectors.get(done).map(|x| x.segments.clone()),
        "lap": q.map(|q| times(&q.sectors)),
        "prev": s.quali.last_best(&p.num).map(times),
        "best": q.and_then(|q| parse_laptime(&q.best)),
        "field": fastest,
        "p1": p1,
        "others": others,
        "live": live,
        "pred": p.pred,
        "proj": p.projected,
    })
}

/// Replays one session and returns its push laps with their results.
fn replay(rt: &tokio::runtime::Runtime, path: &str) -> Vec<Lap> {
    let files = rt.block_on(async {
        let client = archive::http_client().unwrap();
        archive::fetch_session(
            &client,
            token().as_deref(),
            path,
            &feed::topics(false),
            &cache(),
        )
        .await
    });
    let files = match files {
        Ok(f) => f,
        Err(e) => {
            eprintln!("skipping {path}: {e:#}");
            return Vec::new();
        }
    };
    let name = path
        .split('/')
        .nth(1)
        .unwrap_or(path)
        .split('_')
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ")
        + if path.contains("Sprint") { " SQ" } else { " Q" };

    let mut s = State::default();
    let mut last = Duration::ZERO;
    // Laps on, by car and start; laps over the line waiting to settle.
    let mut open: HashMap<(String, DateTime<Utc>), Lap> = HashMap::new();
    let mut waiting: Vec<Lap> = Vec::new();
    let mut done: Vec<Lap> = Vec::new();

    for m in files.open().unwrap() {
        let off = m.offset;
        s.apply(m);
        if off.saturating_sub(last) < FRAME {
            continue;
        }
        last = off;
        s.derive(super::derive::Changed::all(), TrendMode::Avg3);
        let Some(now) = s.feed_utc else { continue };
        let v = &s.views;
        let part = v.session.part.unwrap_or(0);

        for p in &v.pushing {
            let key = (p.num.clone(), p.start);
            let row = v.tower.iter().find(|r| r.num == p.num);
            match &p.end {
                None => {
                    let lap = open.entry(key).or_insert_with(|| Lap {
                        session: name.clone(),
                        part,
                        tla: p.tla.clone(),
                        time: row
                            .and_then(|r| r.quali.as_ref())
                            .map(|q| q.best.clone())
                            .unwrap_or_default(),
                        ..Lap::default()
                    });
                    let n = p
                        .sectors
                        .iter()
                        .take_while(|x| !x.value.is_empty())
                        .count()
                        .min(2);
                    lap.at[n] = p.pred;
                    lap.proj[n] = p.projected;
                    lap.frames.push(p.pred);
                    if n > 0 {
                        let f = features(&s, v, p);
                        lap.first[n].get_or_insert_with(|| f.clone());
                        lap.last[n] = Some(f);
                    }
                    if n == 2 {
                        lap.best_at2 = row.and_then(best);
                        lap.others_at2 = v
                            .tower
                            .iter()
                            .filter(|r| !r.out && r.num != p.num)
                            .filter_map(best)
                            .collect();
                    }
                }
                Some((end, time, _)) => {
                    if let Some(mut lap) = open.remove(&key) {
                        let before = parse_laptime(&lap.time);
                        lap.improved =
                            parse_laptime(time).is_some_and(|t| before.is_none_or(|b| t < b));
                        lap.time = time.clone();
                        lap.sectors = times(&p.sectors);
                        lap.end = Some(*end);
                        waiting.push(lap);
                    }
                }
            }
        }
        // A lap that left the list without reaching the line was abandoned.
        open.retain(|(num, start), _| v.pushing.iter().any(|p| &p.num == num && p.start == *start));

        waiting.retain_mut(|lap| {
            if lap.part != part {
                return false;
            }
            if now < lap.end.unwrap() + SETTLE {
                return true;
            }
            lap.result = v.tower.iter().find(|r| r.tla == lap.tla).map(|r| r.pos);
            done.push(lap.clone());
            false
        });
    }
    done.retain(|l| l.result.is_some());
    done
}

#[test]
#[ignore = "downloads archive data"]
fn quali_bench() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let only = std::env::var("BENCH_ONLY").ok();
    let per_session = only.is_some() || std::env::var("BENCH_SESSIONS").is_ok();
    let mut all = Score::default();
    let mut improving = Score::default();
    let mut jsonl = String::new();
    for path in sessions(&rt) {
        if only.as_deref().is_some_and(|o| !path.contains(o)) {
            continue;
        }
        let laps = replay(&rt, &path);
        let mut session = Score::default();
        for l in &laps {
            session.add(l);
            all.add(l);
            if l.improved {
                improving.add(l);
            }
            let line = json!({
                "session": l.session,
                "part": l.part,
                "tla": l.tla,
                "result": l.result,
                "time": parse_laptime(&l.time),
                "improved": l.improved,
                "sectors": l.sectors,
                "f1": l.first[1], "s1": l.last[1],
                "f2": l.first[2], "s2": l.last[2],
            });
            writeln!(jsonl, "{line}").unwrap();
        }
        if per_session && let Some(l) = laps.first() {
            eprintln!("{}", session.line(&l.session));
        }
    }
    eprintln!("{}", all.line("ALL"));
    eprintln!("{}", improving.line("IMPROVING"));
    std::fs::write(cache().join("quali_bench.jsonl"), jsonl).unwrap();
}
