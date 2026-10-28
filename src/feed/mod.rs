//! Feed sources. Live and replay both produce [`FeedEvent`]s into one
//! channel, so everything after the source is exercised by replay.

pub mod archive;
pub mod live;
pub mod record;
pub mod replay;
pub mod signalr;
pub mod stream;

use std::io::Read;
use std::time::{Duration, Instant};

use anyhow::Context;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

/// Topics that work without an F1 TV token.
pub const BASE_TOPICS: &[&str] = &[
    "Heartbeat",
    "SessionInfo",
    "SessionStatus",
    "ExtrapolatedClock",
    "DriverList",
    "TimingData",
    "TimingAppData",
    "TrackStatus",
    "RaceControlMessages",
    "LapCount",
    "WeatherData",
    "ChampionshipPrediction",
];

/// Compressed, auth-gated topics. Only needed for the map and the
/// speed-based stopped-car rule.
pub const Z_TOPICS: &[&str] = &["Position.z", "CarData.z"];

pub fn topics(with_z: bool) -> Vec<&'static str> {
    let mut t = BASE_TOPICS.to_vec();
    if with_z {
        t.extend_from_slice(Z_TOPICS);
    }
    t
}

/// One topic update. `offset` is the time since the start of the archive
/// stream or recording; `utc` is the feed's own timestamp when it has one.
#[derive(Debug, Clone)]
pub struct FeedMessage {
    pub topic: String,
    pub data: Value,
    pub offset: Duration,
    pub utc: Option<DateTime<Utc>>,
}

#[derive(Debug)]
pub enum FeedEvent {
    /// Full state keyed by topic; replaces everything held so far.
    Snapshot(Map<String, Value>),
    Message(FeedMessage),
    /// Replay is about to re-feed from the start (backwards seek).
    Reset,
    Replay(ReplayStatus),
    Conn(ConnStatus),
}

/// Where replay playback is. The app extrapolates the current position
/// from `pos` at `at` while not paused.
#[derive(Debug, Clone, Copy)]
pub struct ReplayStatus {
    pub pos: Duration,
    pub at: Instant,
    pub speed: f64,
    pub paused: bool,
    pub end: Duration,
}

impl ReplayStatus {
    pub fn current(&self) -> Duration {
        if self.paused {
            return self.pos;
        }
        (self.pos + self.at.elapsed().mul_f64(self.speed)).min(self.end)
    }
}

#[derive(Debug, Clone)]
pub enum ConnStatus {
    Connecting,
    Connected,
    Disconnected(String),
}

/// Decodes a `.z` payload: a JSON string of base64 over raw deflate.
pub fn decode_z(data: &Value) -> anyhow::Result<Value> {
    let s = data
        .as_str()
        .context("compressed payload is not a string")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .context("bad base64")?;
    let mut out = String::new();
    flate2::read::DeflateDecoder::new(&bytes[..])
        .read_to_string(&mut out)
        .context("bad deflate")?;
    Ok(serde_json::from_str(&out)?)
}

/// Formats an offset the way archive `.jsonStream` lines do: `HH:MM:SS.mmm`.
pub fn fmt_offset(d: Duration) -> String {
    let ms = d.as_millis();
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

/// Parses `HH:MM:SS.mmm`, `MM:SS` or plain seconds.
pub fn parse_offset(s: &str) -> Option<Duration> {
    let mut ms: u64 = 0;
    let mut parts = s.trim().rsplit(':');
    let secs = parts.next()?.parse::<f64>().ok()?;
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    ms += (secs * 1000.0).round() as u64;
    let mut unit = 60_000;
    for part in parts {
        ms += part.parse::<u64>().ok()? * unit;
        unit *= 60;
    }
    Some(Duration::from_millis(ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_real_position_payload() {
        let raw = Value::String("7ZOxCsIwEIbf5eZU7i5JjdmdFeygFYciHYK0lTZOJe9u9QXMTS5dfgh8w+Xn/2Y4DlOIYejBX2eoQtdOseme4IGRbYG6oLJC7Ym8tRtjHKOpQcG+j2NoJ/Az0CdOsYmv5QmHvhqb+2NBzuBRweWb9ZJJgclHbT5a5qPbfJRQwLKAFZRAgq+Ry2dZcC9rASvoVwv61ZKNSUYmWZmgBye412V3lpL6rahDR2bHq6Oro6uj/3D0lt4=".into());
        let v = decode_z(&raw).unwrap();
        let entries = &v["Position"][0]["Entries"];
        assert!(entries.as_object().unwrap().len() >= 20);
        assert!(entries["1"]["X"].is_number());
    }

    #[test]
    fn offsets_round_trip() {
        let d = parse_offset("01:12:03.456").unwrap();
        assert_eq!(fmt_offset(d), "01:12:03.456");
        assert_eq!(parse_offset("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_offset("1:30").unwrap(), Duration::from_secs(90));
        assert!(parse_offset("x").is_none());
    }
}
