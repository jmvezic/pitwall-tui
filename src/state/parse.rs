//! Parsers for the string formats the feed uses.

use serde_json::Value;

/// An interval or gap: `"+1.234"`, `"1L"`, `"2 L"`, or the leader's
/// `"LAP 23"` / empty string.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gap {
    Leader,
    Seconds(f64),
    Laps(u32),
    None,
}

pub fn parse_gap(s: &str) -> Gap {
    let s = s.trim();
    if s.is_empty() {
        return Gap::None;
    }
    if s.to_ascii_uppercase().starts_with("LAP") {
        return Gap::Leader;
    }
    let body = s.trim_start_matches('+');
    if let Some(n) = body.strip_suffix(['L', 'l']) {
        return n.trim().parse().map_or(Gap::None, Gap::Laps);
    }
    body.parse().map_or(Gap::None, Gap::Seconds)
}

impl Gap {
    pub fn seconds(self) -> Option<f64> {
        match self {
            Gap::Seconds(s) => Some(s),
            Gap::Leader => Some(0.0),
            _ => None,
        }
    }
}

/// Parses `"1:32.456"` or `"58.123"` into seconds.
pub fn parse_laptime(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total = 0.0;
    for part in s.split(':') {
        total = total * 60.0 + part.parse::<f64>().ok()?;
    }
    (total > 0.0).then_some(total)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Compound {
    Soft,
    Medium,
    Hard,
    Intermediate,
    Wet,
    Unknown,
}

impl Compound {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_uppercase().as_str() {
            "SOFT" | "HYPERSOFT" | "ULTRASOFT" | "SUPERSOFT" => Self::Soft,
            "MEDIUM" => Self::Medium,
            "HARD" => Self::Hard,
            "INTERMEDIATE" => Self::Intermediate,
            "WET" => Self::Wet,
            _ => Self::Unknown,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Soft => "SOFT",
            Self::Medium => "MEDIUM",
            Self::Hard => "HARD",
            Self::Intermediate => "INTERS",
            Self::Wet => "FULLWET",
            Self::Unknown => "?",
        }
    }
}

/// The feed sends some booleans as strings (`"New": "true"`).
pub fn as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.as_str() {
            "true" | "True" | "1" => Some(true),
            "false" | "False" | "0" => Some(false),
            _ => None,
        },
        Value::Number(n) => n.as_u64().map(|n| n != 0),
        _ => None,
    }
}

/// Numbers sometimes arrive as strings (`"Position": "3"`).
pub fn as_u32(v: &Value) -> Option<u32> {
    match v {
        Value::Number(n) => n.as_u64().map(|n| n as u32),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Numbers sometimes arrive as strings (`"AirTemp": "31.5"`).
pub fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub fn as_str(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

/// `"FF8000"` → RGB.
pub fn parse_colour(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(s, 16).ok()?;
    Some(((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gaps() {
        assert_eq!(parse_gap("+1.234"), Gap::Seconds(1.234));
        assert_eq!(parse_gap("0.456"), Gap::Seconds(0.456));
        assert_eq!(parse_gap("1L"), Gap::Laps(1));
        assert_eq!(parse_gap("2 L"), Gap::Laps(2));
        assert_eq!(parse_gap("+13L"), Gap::Laps(13));
        assert_eq!(parse_gap("LAP 57"), Gap::Leader);
        assert_eq!(parse_gap(""), Gap::None);
        assert_eq!(parse_gap("-"), Gap::None);
    }

    #[test]
    fn laptimes() {
        assert_eq!(parse_laptime("1:32.456"), Some(92.456));
        assert_eq!(parse_laptime("58.123"), Some(58.123));
        assert_eq!(
            parse_laptime("1:15.096").map(|t| (t * 1000.0).round()),
            Some(75096.0)
        );
        assert_eq!(parse_laptime(""), None);
        assert_eq!(parse_laptime("abc"), None);
    }

    #[test]
    fn compounds() {
        assert_eq!(Compound::parse("SOFT"), Compound::Soft);
        assert_eq!(Compound::parse("INTERMEDIATE").name(), "INTERS");
        assert_eq!(Compound::parse("UNKNOWN"), Compound::Unknown);
        assert_eq!(Compound::parse("TEST_UNKNOWN"), Compound::Unknown);
    }

    #[test]
    fn stringly_values() {
        assert_eq!(as_bool(&json!("true")), Some(true));
        assert_eq!(as_bool(&json!("false")), Some(false));
        assert_eq!(as_bool(&json!(true)), Some(true));
        assert_eq!(as_u32(&json!("17")), Some(17));
        assert_eq!(as_u32(&json!(17)), Some(17));
        assert_eq!(parse_colour("FF8000"), Some((255, 128, 0)));
        assert_eq!(parse_colour(""), None);
    }
}
