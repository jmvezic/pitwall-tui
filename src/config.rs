//! Config file, data paths and the F1 TV token.

use std::path::{Path, PathBuf};

use anyhow::Context;
use base64::Engine;
use chrono::{DateTime, TimeDelta, Utc};
use serde::Deserialize;

const TEMPLATE: &str = r#"# pitwall-tui configuration

# F1 TV subscription token. Sign in at https://f1tv.formula1.com, then copy
# the value of the `login-session` cookie. You can paste either the whole
# cookie value or just its data.subscriptionToken JWT. Without a token, live
# car positions and speed-based stopped-car detection are unavailable.
# token = ""

# Send the token as an access_token query parameter instead of an
# Authorization header.
# token_in_query = false

# Use car positions and telemetry (Position.z / CarData.z) when available.
# Without them the lap line places cars by timing mini-sector.
# map = true

# Live: hold data back this many seconds to stay in step with a video
# stream. Set in the app with s (sync at lights out) and + / -.
# delay = 0
"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub token: Option<String>,
    #[serde(default)]
    pub token_in_query: bool,
    #[serde(default = "yes")]
    pub map: bool,
    pub cache_dir: Option<PathBuf>,
    pub record_dir: Option<PathBuf>,
    /// Live delay in seconds.
    #[serde(default)]
    pub delay: u64,
}

fn yes() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            token: None,
            token_in_query: false,
            map: true,
            cache_dir: None,
            record_dir: None,
            delay: 0,
        }
    }
}

pub struct Paths {
    pub config_file: PathBuf,
    pub cache: PathBuf,
    pub logs: PathBuf,
    pub recordings: PathBuf,
    /// The browser profile `pitwall login` signs in with.
    pub signin_profile: PathBuf,
}

impl Paths {
    pub fn new(cfg: &Config) -> Self {
        let dirs = directories::ProjectDirs::from("", "", "pitwall");
        let (config, cache, data) = match &dirs {
            Some(d) => (
                d.config_dir().to_owned(),
                d.cache_dir().to_owned(),
                d.data_dir().to_owned(),
            ),
            None => {
                let base = PathBuf::from(".pitwall");
                (base.clone(), base.join("cache"), base)
            }
        };
        Self {
            config_file: config.join("config.toml"),
            cache: cfg.cache_dir.clone().unwrap_or(cache),
            logs: data.join("logs"),
            recordings: cfg
                .record_dir
                .clone()
                .unwrap_or_else(|| data.join("recordings")),
            signin_profile: data.join("signin-browser"),
        }
    }
}

pub fn default_config_file() -> PathBuf {
    Paths::new(&Config::default()).config_file
}

/// Loads the config, writing a commented template on first run.
pub fn load(path: &Path) -> anyhow::Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let mut cfg: Config =
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            cfg.token = cfg.token.as_deref().and_then(normalise_token);
            Ok(cfg)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(path, TEMPLATE);
            Ok(Config::default())
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes `token = "..."` into the config file, replacing an existing token
/// line (or the template's commented one) and keeping everything else.
/// The file is made readable by the owner only.
pub fn save_token(path: &Path, token: &str) -> anyhow::Result<()> {
    save_setting(path, "token", toml::Value::String(token.to_owned()))
}

/// Writes `delay = N` into the config file, as [`save_token`] does.
pub fn save_delay(path: &Path, secs: u64) -> anyhow::Result<()> {
    save_setting(path, "delay", toml::Value::Integer(secs as i64))
}

fn save_setting(path: &Path, key: &str, value: toml::Value) -> anyhow::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TEMPLATE.to_owned(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let new_line = format!("{key} = {value}");
    let is_key = |l: &str, commented: bool| {
        let l = l.trim_start();
        let l = if commented {
            match l.strip_prefix('#') {
                Some(rest) => rest.trim_start(),
                None => return false,
            }
        } else {
            l
        };
        l.strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    };
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    if let Some(i) = lines
        .iter()
        .position(|l| is_key(l, false))
        .or_else(|| lines.iter().position(|l| is_key(l, true)))
    {
        lines[i] = new_line;
    } else {
        lines.push(new_line);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    toml::from_str::<Config>(&out).context("updated config doesn't parse")?;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Accepts the bare JWT or the whole URL-encoded `login-session` cookie.
pub fn normalise_token(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let decoded = percent_decode(raw);
    if decoded.starts_with('{')
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&decoded)
        && let Some(t) = v["data"]["subscriptionToken"].as_str()
    {
        return Some(t.to_owned());
    }
    Some(raw.to_owned())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(h) = s.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(h, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The JWT `exp` claim.
pub fn token_expiry(token: &str) -> Option<DateTime<Utc>> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    DateTime::from_timestamp(v["exp"].as_i64()?, 0)
}

/// Warn about the sign-in this long before the token expires.
const EXPIRY_WARNING: TimeDelta = TimeDelta::hours(4);

/// Where the F1 TV sign-in stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignIn {
    Missing,
    /// The token's expiry can't be read.
    Unreadable,
    Valid,
    /// Expires within [`EXPIRY_WARNING`]; the time left.
    Expiring(TimeDelta),
    Expired,
}

impl SignIn {
    pub fn of(token: Option<&str>, now: DateTime<Utc>) -> Self {
        match token.map(token_expiry) {
            None => Self::Missing,
            Some(None) => Self::Unreadable,
            Some(Some(exp)) => Self::at(exp, now),
        }
    }

    pub fn at(expires: DateTime<Utc>, now: DateTime<Utc>) -> Self {
        let left = expires - now;
        if left <= TimeDelta::zero() {
            Self::Expired
        } else if left < EXPIRY_WARNING {
            Self::Expiring(left)
        } else {
            Self::Valid
        }
    }

    /// Whether the token can't be used.
    pub fn is_dead(self) -> bool {
        matches!(self, Self::Unreadable | Self::Expired)
    }

    /// What a command says when it starts, if anything is wrong.
    pub fn message(self) -> Option<String> {
        match self {
            Self::Missing => Some(
                "You're not signed in to F1 TV, please sign in by running: pitwall login".into(),
            ),
            Self::Unreadable => Some(
                "Your F1 TV sign-in can't be read, please sign in again by running: pitwall login"
                    .into(),
            ),
            Self::Valid => None,
            Self::Expiring(left) => Some(format!(
                "Your F1 TV sign-in is about to expire in {}, please sign in again to avoid \
                 interruptions by running: pitwall login",
                fmt_left(left)
            )),
            Self::Expired => Some(
                "Your F1 TV sign-in expired, please sign in again by running: pitwall login".into(),
            ),
        }
    }

    /// The same, short enough for the app's status line, where `l` signs in.
    pub fn notice(self) -> Option<String> {
        match self {
            Self::Missing | Self::Valid => None,
            Self::Unreadable => Some("F1 TV sign-in unreadable: press l to sign in".into()),
            Self::Expiring(left) => Some(format!(
                "F1 TV sign-in expires in {}: press l to sign in",
                fmt_left(left)
            )),
            Self::Expired => Some("F1 TV sign-in expired: press l to sign in".into()),
        }
    }
}

/// `3h 12m`, or `47m` under an hour; never less than a minute.
fn fmt_left(left: TimeDelta) -> String {
    let mins = left.num_minutes().max(1);
    if mins < 60 {
        format!("{mins}m")
    } else {
        format!("{}h {:02}m", mins / 60, mins % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(exp: i64) -> String {
        let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.sig",
            e.encode(r#"{"alg":"none"}"#),
            e.encode(format!(r#"{{"exp":{exp}}}"#))
        )
    }

    #[test]
    fn warns_near_expiry() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let in_secs = |s: i64| SignIn::of(Some(&jwt(1_800_000_000 + s)), now);
        assert_eq!(in_secs(86400), SignIn::Valid);
        assert_eq!(in_secs(4 * 3600), SignIn::Valid);
        assert_eq!(
            in_secs(3 * 3600 + 12 * 60),
            SignIn::Expiring(TimeDelta::minutes(192))
        );
        assert_eq!(in_secs(0), SignIn::Expired);
        assert_eq!(SignIn::of(Some("nope"), now), SignIn::Unreadable);
        assert_eq!(SignIn::of(None, now), SignIn::Missing);

        assert!(
            in_secs(3 * 3600 + 12 * 60)
                .message()
                .unwrap()
                .contains("about to expire in 3h 12m")
        );
        assert_eq!(
            in_secs(47 * 60 + 30).notice().unwrap(),
            "F1 TV sign-in expires in 47m: press l to sign in"
        );
        assert_eq!(
            in_secs(-5).message().unwrap(),
            "Your F1 TV sign-in expired, please sign in again by running: pitwall login"
        );
        assert_eq!(in_secs(86400).message(), None);
        assert_eq!(SignIn::Missing.notice(), None);
    }

    #[test]
    fn extracts_token_from_cookie() {
        let cookie = "%7B%22data%22%3A%7B%22subscriptionToken%22%3A%22abc.def.ghi%22%7D%7D";
        assert_eq!(normalise_token(cookie).as_deref(), Some("abc.def.ghi"));
        assert_eq!(
            normalise_token(" abc.def.ghi ").as_deref(),
            Some("abc.def.ghi")
        );
        assert_eq!(normalise_token(""), None);
    }

    #[test]
    fn template_parses() {
        let cfg: Config = toml::from_str(TEMPLATE).unwrap();
        assert!(cfg.map);
        assert!(cfg.token.is_none());
    }

    #[test]
    fn save_token_edits_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // First run: the template's commented line becomes the token.
        save_token(&path, "a.b.c").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\ntoken = \"a.b.c\"\n"));
        assert!(text.contains("# map = true"));
        assert_eq!(load(&path).unwrap().token.as_deref(), Some("a.b.c"));

        // Again: replaced, not duplicated; other settings kept.
        std::fs::write(&path, "map = false\ntoken = \"old\"\n# note\n").unwrap();
        save_token(&path, "d.e.f").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "map = false\ntoken = \"d.e.f\"\n# note\n"
        );

        // No token line at all: appended.
        std::fs::write(&path, "map = false\n").unwrap();
        save_token(&path, "g.h.i").unwrap();
        let cfg = load(&path).unwrap();
        assert_eq!(cfg.token.as_deref(), Some("g.h.i"));
        assert!(!cfg.map);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // Other settings the same way, without touching the token.
        save_delay(&path, 32).unwrap();
        let cfg = load(&path).unwrap();
        assert_eq!(cfg.delay, 32);
        assert_eq!(cfg.token.as_deref(), Some("g.h.i"));
    }
}
