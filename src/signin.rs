//! `pitwall login`'s default: opens F1's sign-in page in a Chrome window
//! on pitwall's own browser profile and takes the token from the window's
//! cookies over the DevTools protocol once you have signed in. Unlike
//! reading a browser's cookie store, this needs no Keychain prompt or Full
//! Disk Access.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::config;

/// F1's sign-in form; once signed in it sends the browser on to F1 TV,
/// which sets the `login-session` cookie.
const SIGN_IN_URL: &str =
    "https://account.formula1.com/#/en/login?redirect=https%3A%2F%2Ff1tv.formula1.com%2F";
const COOKIE: &str = "login-session";
const HOST_SUFFIX: &str = "formula1.com";
const WAIT: Duration = Duration::from_secs(600);
/// F1's cookie banner lives in a frame from here and, on a new profile,
/// holds back the sign-in form until it's answered.
const CONSENT_ORIGIN: &str = "https://consent.formula1.com/";
const ESSENTIAL_ONLY: &str = r#"(() => {
  const b = [...document.querySelectorAll('button')]
    .find(e => /^essential only/i.test(e.innerText.trim()));
  if (b) b.click();
  return !!b;
})()"#;

/// Browsers that take Chrome's command-line switches, most likely first.
#[cfg(target_os = "macos")]
const BROWSERS: &[&str] = &[
    "Google Chrome.app/Contents/MacOS/Google Chrome",
    "Brave Browser.app/Contents/MacOS/Brave Browser",
    "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "Chromium.app/Contents/MacOS/Chromium",
];
#[cfg(not(target_os = "macos"))]
const BROWSERS: &[&str] = &[
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
    "brave-browser",
    "microsoft-edge",
];

pub struct Token {
    pub token: String,
    pub expires: DateTime<Utc>,
}

fn find_browser() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let dirs: Vec<PathBuf> = [
        Some(PathBuf::from("/Applications")),
        directories::BaseDirs::new().map(|d| d.home_dir().join("Applications")),
    ]
    .into_iter()
    .flatten()
    .collect();
    #[cfg(not(target_os = "macos"))]
    let dirs: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH")?).collect();
    BROWSERS
        .iter()
        .flat_map(|b| dirs.iter().map(move |d| d.join(b)))
        .find(|p| p.is_file())
}

/// Whether a browser to sign in with is installed.
pub fn available() -> bool {
    find_browser().is_some()
}

/// Opens the sign-in page in a browser window using `profile` and waits
/// until you have signed in. Dropping the future closes the window.
pub async fn sign_in(profile: &Path) -> anyhow::Result<Token> {
    let Some(exe) = find_browser() else {
        bail!("no Chrome, Brave, Edge or Chromium found to sign in with");
    };
    std::fs::create_dir_all(profile).with_context(|| format!("creating {}", profile.display()))?;
    // The browser writes its debugging port here once it is listening.
    let port_file = profile.join("DevToolsActivePort");
    let _ = std::fs::remove_file(&port_file);
    let mut child = Command::new(&exe)
        .kill_on_drop(true)
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
            // Keep macOS from asking for Keychain access for this profile.
            "--use-mock-keychain",
            SIGN_IN_URL,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {}", exe.display()))?;

    let res = wait_for_token(&mut child, &port_file).await;
    // On success the window was asked to close; give it a moment, then make
    // sure it's gone.
    if tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    res
}

async fn wait_for_token(child: &mut Child, port_file: &Path) -> anyhow::Result<Token> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let (port, path) = loop {
        if let Some(p) = std::fs::read_to_string(port_file)
            .ok()
            .and_then(|t| parse_port_file(&t))
        {
            break p;
        }
        if child.try_wait()?.is_some() {
            bail!(
                "the browser exited at once; if a pitwall sign-in window is already open, \
                 close it and try again"
            );
        }
        if Instant::now() > deadline {
            bail!("the browser didn't open its debugging port");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}{path}"))
        .await
        .context("connecting to the browser")?;
    let mut cdp = Cdp { ws, id: 0 };

    // A token the profile already had is not the one we're waiting for.
    let seen: Vec<String> = tokens(&cdp.call("Storage.getCookies").await?)
        .into_iter()
        .map(|t| t.token)
        .collect();
    let deadline = Instant::now() + WAIT;
    // The page we're attached to, by target and session id.
    let mut attached: Option<(String, String)> = None;
    let mut banner_answered = false;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        // On macOS the browser keeps running after its last window closes,
        // and then can't list cookies, so check for a window first.
        let targets = cdp.call("Target.getTargets").await?;
        let page = targets["targetInfos"]
            .as_array()
            .and_then(|ts| ts.iter().find(|t| t["type"] == "page"))
            .and_then(|t| t["targetId"].as_str())
            .map(str::to_owned);
        let Some(page) = page else {
            let _ = cdp.call("Browser.close").await;
            bail!("the sign-in window was closed before signing in");
        };
        if !banner_answered {
            let session = match &attached {
                Some((id, s)) if *id == page => s.clone(),
                _ => {
                    let r = cdp
                        .call_in(
                            None,
                            "Target.attachToTarget",
                            json!({ "targetId": page, "flatten": true }),
                        )
                        .await?;
                    let s = r["sessionId"].as_str().unwrap_or_default().to_owned();
                    attached = Some((page, s.clone()));
                    s
                }
            };
            // Best effort: if it fails, the banner is still there to click.
            banner_answered = answer_cookie_banner(&mut cdp, &session)
                .await
                .unwrap_or(false);
        }
        let cookies = cdp.call("Storage.getCookies").await?;
        if let Some(t) = fresh_token(&cookies, &seen, Utc::now()) {
            let _ = cdp.call("Browser.close").await;
            return Ok(t);
        }
        if Instant::now() > deadline {
            let _ = cdp.call("Browser.close").await;
            bail!(
                "gave up waiting for the sign-in after {} minutes",
                WAIT.as_secs() / 60
            );
        }
    }
}

/// Answers F1's cookie banner with "Essential only cookies" if it's
/// showing in the page `session` is attached to; returns whether it was.
async fn answer_cookie_banner(cdp: &mut Cdp, session: &str) -> anyhow::Result<bool> {
    let tree = cdp
        .call_in(Some(session), "Page.getFrameTree", json!({}))
        .await?;
    for frame in consent_frames(&tree["frameTree"]) {
        let world = cdp
            .call_in(
                Some(session),
                "Page.createIsolatedWorld",
                json!({ "frameId": frame }),
            )
            .await?;
        let r = cdp
            .call_in(
                Some(session),
                "Runtime.evaluate",
                json!({
                    "expression": ESSENTIAL_ONLY,
                    "contextId": world["executionContextId"],
                    "returnByValue": true,
                }),
            )
            .await?;
        if r["result"]["value"] == true {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The ids of the frames in a `Page.getFrameTree` node that come from
/// F1's consent host.
fn consent_frames(node: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    let frame = &node["frame"];
    if frame["url"]
        .as_str()
        .is_some_and(|u| u.starts_with(CONSENT_ORIGIN))
        && let Some(id) = frame["id"].as_str()
    {
        ids.push(id.to_owned());
    }
    for child in node["childFrames"].as_array().into_iter().flatten() {
        ids.extend(consent_frames(child));
    }
    ids
}

/// `DevToolsActivePort`: the port on the first line, the browser's
/// WebSocket path on the second.
fn parse_port_file(text: &str) -> Option<(u16, String)> {
    let mut lines = text.lines();
    let port = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    path.starts_with('/').then(|| (port, path.to_owned()))
}

/// The F1 TV tokens in a `Storage.getCookies` result.
fn tokens(cookies: &Value) -> Vec<Token> {
    cookies["cookies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| {
            c["name"] == COOKIE
                && c["domain"]
                    .as_str()
                    .is_some_and(|d| d.ends_with(HOST_SUFFIX))
        })
        .filter_map(|c| {
            let token = config::normalise_token(c["value"].as_str()?)?;
            let expires = config::token_expiry(&token)?;
            Some(Token { token, expires })
        })
        .collect()
}

/// The unexpired token that wasn't there before, if one has appeared.
fn fresh_token(cookies: &Value, seen: &[String], now: DateTime<Utc>) -> Option<Token> {
    tokens(cookies)
        .into_iter()
        .filter(|t| t.expires > now && !seen.contains(&t.token))
        .max_by_key(|t| t.expires)
}

/// A minimal DevTools protocol client: one call at a time, events ignored.
struct Cdp {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    id: u64,
}

impl Cdp {
    /// A browser-level call without parameters.
    async fn call(&mut self, method: &str) -> anyhow::Result<Value> {
        self.call_in(None, method, json!({})).await
    }

    /// A call in an attached target's `session`, or the browser's.
    async fn call_in(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> anyhow::Result<Value> {
        self.id += 1;
        let mut msg = json!({ "id": self.id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = s.into();
        }
        self.ws.send(Message::text(msg.to_string())).await?;
        while let Some(msg) = self.ws.next().await {
            let Message::Text(text) = msg? else {
                continue;
            };
            let mut v: Value = serde_json::from_str(text.as_str())?;
            if v["id"] == self.id {
                if let Some(e) = v.get("error") {
                    bail!("{method}: {e}");
                }
                return Ok(v["result"].take());
            }
        }
        bail!("the sign-in window was closed before signing in")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn jwt(exp: i64) -> String {
        let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.sig",
            e.encode(r#"{"alg":"none"}"#),
            e.encode(format!(r#"{{"exp":{exp}}}"#))
        )
    }

    fn cookie(domain: &str, name: &str, token: &str) -> Value {
        let value = format!("%7B%22data%22%3A%7B%22subscriptionToken%22%3A%22{token}%22%7D%7D");
        json!({ "domain": domain, "name": name, "value": value })
    }

    #[test]
    fn parses_port_file() {
        assert_eq!(
            parse_port_file("51234\n/devtools/browser/abc\n"),
            Some((51234, "/devtools/browser/abc".into()))
        );
        assert_eq!(parse_port_file("51234\n"), None);
        assert_eq!(parse_port_file("x\n/devtools\n"), None);
    }

    #[test]
    fn finds_consent_frames() {
        let tree = json!({
            "frame": { "id": "top", "url": "https://account.formula1.com/" },
            "childFrames": [
                { "frame": { "id": "stripe", "url": "https://js.stripe.com/v3/" } },
                { "frame": { "id": "app", "url": "https://static.formula1.com/account/" },
                  "childFrames": [
                    { "frame": { "id": "banner", "url": "https://consent.formula1.com/index.html?x=1" } },
                  ] },
                { "frame": { "id": "fake", "url": "https://consent.formula1.com.evil.com/" } },
            ],
        });
        assert_eq!(consent_frames(&tree), vec!["banner".to_owned()]);
    }

    #[test]
    fn waits_for_a_new_unexpired_token() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let old = jwt(1_800_000_000 + 86400);
        let expired = jwt(1_700_000_000);
        let new = jwt(1_800_000_000 + 4 * 86400);
        let seen = vec![old.clone()];

        let before = json!({ "cookies": [cookie(".formula1.com", COOKIE, &old)] });
        assert!(fresh_token(&before, &seen, now).is_none());

        let after = json!({ "cookies": [
            cookie(".formula1.com", COOKIE, &old),
            cookie(".formula1.com", COOKIE, &expired),
            cookie(".formula1.com", "other", &new),
            cookie("evil.com", COOKIE, &new),
        ] });
        assert!(fresh_token(&after, &seen, now).is_none());

        let signed_in = json!({ "cookies": [cookie(".formula1.com", COOKIE, &new)] });
        let t = fresh_token(&signed_in, &seen, now).unwrap();
        assert_eq!(t.token, new);
        assert_eq!(t.expires.timestamp(), 1_800_000_000 + 4 * 86400);
    }
}
