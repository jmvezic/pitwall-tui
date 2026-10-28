//! SignalR Core JSON protocol framing.

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};

/// Record separator that terminates every SignalR JSON frame.
pub const RS: char = '\u{1e}';

#[derive(Debug, PartialEq)]
pub enum Frame {
    /// Completion of the Subscribe call: the full state keyed by topic.
    Snapshot(Map<String, Value>),
    Feed {
        topic: String,
        data: Value,
        ts: Option<DateTime<Utc>>,
    },
    Ping,
    Close(Option<String>),
    /// Handshake response, errors and frame types we don't use.
    Other(Value),
}

pub fn handshake() -> String {
    format!("{}{RS}", json!({"protocol": "json", "version": 1}))
}

pub fn subscribe(topics: &[&str]) -> String {
    let msg = json!({
        "type": 1,
        "target": "Subscribe",
        "arguments": [topics],
        "invocationId": "0",
    });
    format!("{msg}{RS}")
}

pub fn ping() -> String {
    format!("{}{RS}", json!({"type": 6}))
}

/// Splits one WebSocket text message into frames. One message can hold
/// several frames; unparseable ones are logged and skipped.
pub fn parse_frames(text: &str) -> Vec<Frame> {
    text.split(RS)
        .filter(|s| !s.trim().is_empty())
        .filter_map(|s| match serde_json::from_str::<Value>(s) {
            Ok(v) => Some(classify(v)),
            Err(e) => {
                tracing::warn!("unparseable SignalR frame ({e}): {}", truncate(s, 200));
                None
            }
        })
        .collect()
}

fn classify(mut v: Value) -> Frame {
    match v.get("type").and_then(Value::as_u64) {
        Some(1) if v["target"] == "feed" => {
            let Some(Value::Array(args)) = v.get_mut("arguments").map(Value::take) else {
                return Frame::Other(v);
            };
            let mut args = args.into_iter();
            let (Some(Value::String(topic)), Some(data)) = (args.next(), args.next()) else {
                return Frame::Other(Value::Null);
            };
            let ts = args
                .next()
                .and_then(|t| t.as_str().and_then(|s| s.parse::<DateTime<Utc>>().ok()));
            Frame::Feed { topic, data, ts }
        }
        Some(3) if v["invocationId"] == "0" => match v.get_mut("result").map(Value::take) {
            Some(Value::Object(m)) => Frame::Snapshot(m),
            _ => Frame::Other(v),
        },
        Some(6) => Frame::Ping,
        Some(7) => Frame::Close(v["error"].as_str().map(str::to_owned)),
        _ => Frame::Other(v),
    }
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_mixed_frames() {
        let text = format!(
            "{{}}{RS}{{\"type\":6}}{RS}\
             {{\"type\":1,\"target\":\"feed\",\"arguments\":[\"TrackStatus\",{{\"Status\":\"2\",\"Message\":\"Yellow\"}},\"2025-03-16T04:19:27.12Z\"]}}{RS}\
             {{\"type\":42,\"foo\":1}}{RS}\
             {{\"type\":3,\"invocationId\":\"0\",\"result\":{{\"LapCount\":{{\"CurrentLap\":3}}}}}}{RS}\
             not json{RS}\
             {{\"type\":7,\"error\":\"bye\"}}{RS}"
        );
        let frames = parse_frames(&text);
        assert_eq!(frames.len(), 6);
        assert!(matches!(frames[0], Frame::Other(_)));
        assert_eq!(frames[1], Frame::Ping);
        let Frame::Feed { topic, data, ts } = &frames[2] else {
            panic!("expected feed frame")
        };
        assert_eq!(topic, "TrackStatus");
        assert_eq!(data["Status"], "2");
        assert!(ts.is_some());
        assert!(matches!(frames[3], Frame::Other(_)));
        let Frame::Snapshot(snap) = &frames[4] else {
            panic!("expected snapshot")
        };
        assert_eq!(snap["LapCount"]["CurrentLap"], 3);
        assert_eq!(frames[5], Frame::Close(Some("bye".into())));
    }

    #[test]
    fn subscribe_frame_shape() {
        let s = subscribe(&["TimingData", "Position.z"]);
        assert!(s.ends_with(RS));
        let v: Value = serde_json::from_str(s.trim_end_matches(RS)).unwrap();
        assert_eq!(v["arguments"][0][1], "Position.z");
        assert_eq!(v["invocationId"], "0");
    }
}
