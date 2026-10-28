//! Tests against real archived races. They download a few MB per race into
//! `target/test-archive` on first run, so they are ignored by default:
//!
//! ```sh
//! cargo test -- --ignored
//! ```

use std::fmt::Write;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use super::State;
use super::derive::Changed;
use super::laps::TrendMode;
use crate::feed::archive::{self, STATIC_URL};
use crate::feed::{self, FeedMessage, parse_offset};

const AUS: &str = "2025/2025-03-16_Australian_Grand_Prix/2025-03-16_Race/";
const BEL: &str = "2025/2025-07-27_Belgian_Grand_Prix/2025-07-27_Race/";
const IMO: &str = "2025/2025-05-18_Emilia_Romagna_Grand_Prix/2025-05-18_Race/";
const NED: &str = "2025/2025-08-31_Dutch_Grand_Prix/2025-08-31_Race/";

fn cache() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-archive")
}

fn rt() -> tokio::runtime::Runtime {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// The archive wants an F1 TV token: `PITWALL_TOKEN`, else the one
/// `pitwall login` saved.
fn token() -> Option<String> {
    std::env::var("PITWALL_TOKEN").ok().or_else(|| {
        crate::config::load(&crate::config::default_config_file())
            .ok()?
            .token
    })
}

fn load(path: &str) -> Vec<FeedMessage> {
    rt().block_on(async {
        let client = archive::http_client().unwrap();
        archive::fetch_session(
            &client,
            token().as_deref(),
            path,
            &feed::topics(false),
            &cache(),
        )
        .await
        .unwrap()
    })
    .open()
    .unwrap()
    .collect()
}

fn keyframe(path: &str, topic: &str) -> Value {
    let file = cache()
        .join("archive")
        .join(path)
        .join(format!("{topic}.json"));
    let text = std::fs::read_to_string(&file).unwrap_or_else(|_| {
        let text = rt().block_on(async {
            let url = format!("{STATIC_URL}{path}{topic}.json");
            let client = archive::http_client().unwrap();
            archive::get(&client, token().as_deref(), &url)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        });
        std::fs::write(&file, &text).unwrap();
        text
    });
    let mut v = Value::Null;
    super::merge::merge(
        &mut v,
        serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap(),
    );
    v
}

/// Replays up to `until` and derives every view, as the app does.
fn state_at(msgs: &[FeedMessage], until: &str) -> State {
    let until = parse_offset(until).unwrap();
    let mut s = State::default();
    for m in msgs.iter().take_while(|m| m.offset <= until) {
        s.apply(m.clone());
    }
    s.derive(Changed::all(), TrendMode::Avg3);
    s
}

/// A compact text rendering of the derived views for snapshots.
fn summary(s: &State) -> String {
    let v = &s.views;
    let mut out = String::new();
    let ses = &v.session;
    writeln!(
        out,
        "{} | {:?} | {} | lap {:?}/{:?} | track {:?}",
        ses.title, ses.kind, ses.status, ses.lap, ses.total_laps, v.track
    )
    .unwrap();
    writeln!(out, "sector flags: {:?}", v.sector_flags).unwrap();
    for st in &v.stopped {
        writeln!(
            out,
            "STOPPED {} {} lap {} since {}",
            st.num,
            st.tla,
            st.lap,
            st.since.format("%H:%M:%S")
        )
        .unwrap();
    }
    for r in &v.tower {
        let tyre = r
            .tyre
            .as_ref()
            .map(|t| {
                format!(
                    "{}{:>3}{} x{}",
                    &t.compound.name()[..1],
                    t.age,
                    if t.new { ' ' } else { '*' },
                    t.stints
                )
            })
            .unwrap_or_default();
        let trend = r
            .trend
            .map(|t| {
                format!(
                    "{:+.2}{}{}",
                    t.per_lap,
                    if t.from_interval { "i" } else { "" },
                    t.laps_to_catch
                        .map(|n| format!(" ~{n}L"))
                        .unwrap_or_default()
                )
            })
            .unwrap_or_default();
        writeln!(
            out,
            "{:>2} {:<3} {:>8} {:>8} {:>3} {:>9} {:>9} {} {:<11} {:<12} {}",
            r.pos,
            r.tla,
            r.interval,
            r.gap,
            r.laps,
            r.last,
            r.best,
            r.pits,
            tyre,
            trend,
            r.tag.map(|t| format!("{t:?}")).unwrap_or_default()
        )
        .unwrap();
    }
    for m in v.rcm.iter().take(5) {
        writeln!(out, "RCM L{:?} {:?}: {}", m.lap, m.kind, m.text).unwrap();
    }
    out
}

#[test]
#[ignore = "downloads archive data"]
fn merged_topics_match_final_keyframes() {
    for path in [AUS, BEL] {
        let msgs = load(path);
        let end = msgs.last().unwrap().offset;
        let s = state_at(&msgs, &feed::fmt_offset(end + Duration::from_secs(1)));
        for topic in [
            "TimingData",
            "TimingAppData",
            "RaceControlMessages",
            "TrackStatus",
            "LapCount",
        ] {
            let want = keyframe(path, topic);
            let got = &s.topics[topic];
            assert!(
                got == &want,
                "{path}{topic} differs from keyframe:\n{}",
                first_diff(got, &want, String::new()).unwrap_or_default()
            );
        }
    }
}

fn first_diff(a: &Value, b: &Value, at: String) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys()) {
                let (va, vb) = (
                    x.get(k).unwrap_or(&Value::Null),
                    y.get(k).unwrap_or(&Value::Null),
                );
                if let Some(d) = first_diff(va, vb, format!("{at}.{k}")) {
                    return Some(d);
                }
            }
            None
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x
            .iter()
            .zip(y)
            .enumerate()
            .find_map(|(i, (va, vb))| first_diff(va, vb, format!("{at}[{i}]"))),
        _ if a == b => None,
        _ => Some(format!("{at}: got {a} want {b}")),
    }
}

#[test]
#[ignore = "downloads archive data"]
fn australia_safety_car_and_stopped_cars() {
    let msgs = load(AUS);
    insta::assert_snapshot!("aus_sc_lap1", summary(&state_at(&msgs, "01:15:00")));
    insta::assert_snapshot!("aus_green_lap25", summary(&state_at(&msgs, "01:52:00")));
    insta::assert_snapshot!("aus_alonso_stopped", summary(&state_at(&msgs, "02:09:30")));
}

#[test]
#[ignore = "downloads archive data"]
fn belgium_red_flag() {
    let msgs = load(BEL);
    insta::assert_snapshot!("bel_red_flag", summary(&state_at(&msgs, "00:58:00")));
    insta::assert_snapshot!("bel_green_lap25", summary(&state_at(&msgs, "03:05:00")));
}

#[test]
#[ignore = "downloads archive data"]
fn imola_vsc() {
    let msgs = load(IMO);
    insta::assert_snapshot!("imo_vsc", summary(&state_at(&msgs, "01:35:20")));
}

#[test]
#[ignore = "downloads archive data"]
fn zandvoort_vsc_and_sc() {
    let msgs = load(NED);
    insta::assert_snapshot!("ned_vsc", summary(&state_at(&msgs, "01:38:10")));
}
