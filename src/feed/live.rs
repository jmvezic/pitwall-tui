//! Live SignalR Core connection to livetiming.formula1.com.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{AUTHORIZATION, COOKIE, SET_COOKIE, USER_AGENT};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{interval, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::record::Recorder;
use super::signalr::{self, Frame};
use super::{ConnStatus, FeedEvent, FeedMessage};
use crate::config::{self, SignIn};

const NEGOTIATE_URL: &str =
    "https://livetiming.formula1.com/signalrcore/negotiate?negotiateVersion=1";
const CONNECT_URL: &str = "wss://livetiming.formula1.com/signalrcore";
const STATUS_URL: &str = "https://livetiming.formula1.com/static/StreamingStatus.json";
const SESSION_URL: &str = "https://livetiming.formula1.com/static/SessionInfo.json";
/// How often to check whether F1 has brought the feed live or moved it on.
const WATCH_EVERY: Duration = Duration::from_secs(30);
const UA: &str = concat!("pitwall-tui/", env!("CARGO_PKG_VERSION"));
const BACKOFF_SECS: &[u64] = &[1, 2, 5, 10];
/// Reconnect when the socket has been silent this long; Heartbeat arrives
/// every 15 s even when nothing else happens.
const SILENCE_LIMIT: Duration = Duration::from_secs(60);

pub struct LiveOptions {
    pub token: Option<String>,
    /// The config file the token came from, re-read on every connect so a
    /// `pitwall login` during the session takes effect.
    pub token_file: Option<PathBuf>,
    /// Send the token as an `access_token` query parameter instead of a
    /// bearer header.
    pub token_in_query: bool,
    pub topics: Vec<&'static str>,
    pub recorder: Option<Recorder>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Negotiate {
    connection_token: String,
}

/// Keeps a connection open, reconnecting with backoff, until the receiver
/// is dropped.
///
/// A message on `reset` drops the connection and reconnects at once, so the
/// server sends a fresh snapshot (e.g. after it has moved to a new session).
/// It does the same by itself when F1's static files say the feed has gone
/// live or moved to another session, which an open connection can miss.
pub async fn run(
    mut opts: LiveOptions,
    tx: mpsc::Sender<FeedEvent>,
    mut reset: mpsc::Receiver<()>,
) {
    let started = Instant::now();
    let watch = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let mut seen = None;
    let mut attempt = 0;
    loop {
        if tx
            .send(FeedEvent::Conn(ConnStatus::Connecting))
            .await
            .is_err()
        {
            return;
        }
        let t0 = Instant::now();
        let res = tokio::select! {
            res = session(&mut opts, &tx, started) => res,
            () = reset_requested(&mut reset) => {
                tracing::info!("reset: reconnecting");
                attempt = 0;
                continue;
            }
            () = moved_on(&watch, &mut seen) => {
                tracing::info!("feed went live or moved on: reconnecting");
                attempt = 0;
                continue;
            }
        };
        if tx.is_closed() {
            return;
        }
        let why = match res {
            Ok(()) => "closed by server".to_owned(),
            Err(e) => format!("{e:#}"),
        };
        tracing::warn!("live feed disconnected: {why}");
        let _ = tx
            .send(FeedEvent::Conn(ConnStatus::Disconnected(why)))
            .await;
        if t0.elapsed() > Duration::from_secs(60) {
            attempt = 0;
        }
        let wait = Duration::from_secs(BACKOFF_SECS[attempt.min(BACKOFF_SECS.len() - 1)]);
        tokio::select! {
            () = sleep(wait) => attempt += 1,
            () = reset_requested(&mut reset) => attempt = 0,
        }
    }
}

/// Resolves on a reset request; never, once nobody can send one.
async fn reset_requested(reset: &mut mpsc::Receiver<()>) {
    if reset.recv().await.is_none() {
        std::future::pending::<()>().await;
    }
}

/// What F1 says the live feed carries, from its static files.
#[derive(Debug, Clone, PartialEq)]
struct Streaming {
    available: bool,
    /// The session's archive path, which names it.
    session: String,
}

impl Streaming {
    fn parse(status: &Value, info: &Value) -> Streaming {
        Streaming {
            available: status["Status"].as_str() == Some("Available"),
            session: info["Path"].as_str().unwrap_or_default().to_owned(),
        }
    }

    /// Whether the feed has gone live, or moved to another session, since
    /// `was`. Going offline after a session isn't worth a reconnect.
    fn moved_on_from(&self, was: &Streaming) -> bool {
        self.session != was.session || (self.available && !was.available)
    }
}

/// Resolves once F1 brings the feed live or moves it to a new session.
/// `seen` keeps what was last seen across reconnects.
async fn moved_on(client: &reqwest::Client, seen: &mut Option<Streaming>) {
    let mut every = interval(WATCH_EVERY);
    loop {
        every.tick().await;
        let now = match streaming(client).await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("streaming status: {e:#}");
                continue;
            }
        };
        let moved = seen.as_ref().is_some_and(|was| now.moved_on_from(was));
        *seen = Some(now);
        if moved {
            return;
        }
    }
}

async fn streaming(client: &reqwest::Client) -> anyhow::Result<Streaming> {
    let status = get_static(client, STATUS_URL).await?;
    let info = get_static(client, SESSION_URL).await?;
    Ok(Streaming::parse(&status, &info))
}

async fn get_static(client: &reqwest::Client, url: &str) -> anyhow::Result<Value> {
    let body = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    // F1's static files start with a byte-order mark.
    serde_json::from_str(body.trim_start_matches('\u{feff}')).with_context(|| url.to_owned())
}

async fn session(
    opts: &mut LiveOptions,
    tx: &mpsc::Sender<FeedEvent>,
    started: Instant,
) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(20))
        .build()?;
    if let Some(path) = &opts.token_file
        && let Ok(cfg) = config::load(path)
    {
        opts.token = cfg.token;
    }
    // An expired token might get the connection refused; without one the
    // feed still works, minus positions and telemetry.
    let token = opts
        .token
        .as_deref()
        .filter(|t| !SignIn::of(Some(t), Utc::now()).is_dead());
    let mut req = client.post(NEGOTIATE_URL);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await?.error_for_status().context("negotiate")?;
    // The load balancer pins the socket to the negotiating node by cookie.
    let cookies: Vec<String> = resp
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok()?.split(';').next().map(str::to_owned))
        .collect();
    let body = resp.text().await?;
    tracing::info!("negotiate response: {body}");
    let neg: Negotiate = serde_json::from_str(&body).context("negotiate body")?;

    let mut params = vec![("id", neg.connection_token.as_str())];
    if let (true, Some(t)) = (opts.token_in_query, token) {
        params.push(("access_token", t));
    }
    let url = reqwest::Url::parse_with_params(CONNECT_URL, &params)?;
    let mut request = url.as_str().into_client_request()?;
    let headers = request.headers_mut();
    headers.insert(USER_AGENT, UA.parse()?);
    if !cookies.is_empty() {
        headers.insert(COOKIE, cookies.join("; ").parse()?);
    }
    if let (false, Some(t)) = (opts.token_in_query, token) {
        headers.insert(AUTHORIZATION, format!("Bearer {t}").parse()?);
    }

    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .context("websocket connect")?;
    let (mut sink, mut stream) = ws.split();
    sink.send(Message::text(signalr::handshake())).await?;
    sink.send(Message::text(signalr::subscribe(&opts.topics)))
        .await?;
    tx.send(FeedEvent::Conn(ConnStatus::Connected))
        .await
        .map_err(|_| anyhow!("app closed"))?;

    let mut ping = interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            _ = ping.tick() => sink.send(Message::text(signalr::ping())).await?,
            next = timeout(SILENCE_LIMIT, stream.next()) => {
                let msg = next.map_err(|_| anyhow!("no data for {} s", SILENCE_LIMIT.as_secs()))?;
                let Some(msg) = msg else { return Ok(()) };
                let text = match msg? {
                    Message::Text(t) => t,
                    Message::Close(f) => {
                        tracing::info!("websocket close: {f:?}");
                        return Ok(());
                    }
                    _ => continue,
                };
                for frame in signalr::parse_frames(text.as_str()) {
                    handle_frame(frame, opts, tx, started).await?;
                }
            }
        }
    }
}

async fn handle_frame(
    frame: Frame,
    opts: &mut LiveOptions,
    tx: &mpsc::Sender<FeedEvent>,
    started: Instant,
) -> anyhow::Result<()> {
    let offset = started.elapsed();
    let ev = match frame {
        Frame::Snapshot(snap) => {
            tracing::info!("snapshot with {} topics", snap.len());
            if let Some(r) = &mut opts.recorder {
                for (topic, data) in &snap {
                    record(r, topic, offset, data);
                }
            }
            FeedEvent::Snapshot(snap)
        }
        Frame::Feed { topic, data, ts } => {
            if let Some(r) = &mut opts.recorder {
                record(r, &topic, offset, &data);
            }
            FeedEvent::Message(FeedMessage {
                topic,
                data,
                offset,
                utc: ts,
            })
        }
        Frame::Close(err) => return Err(anyhow!("server closed: {}", err.unwrap_or_default())),
        Frame::Ping => return Ok(()),
        Frame::Other(v) => {
            if v.get("error").is_some() {
                tracing::warn!("SignalR error frame: {v}");
            }
            return Ok(());
        }
    };
    tx.send(ev).await.map_err(|_| anyhow!("app closed"))
}

fn record(r: &mut Recorder, topic: &str, offset: Duration, data: &serde_json::Value) {
    if let Err(e) = r.write(topic, offset, data) {
        tracing::error!("recording {topic}: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn streaming(status: &str, path: &str) -> Streaming {
        Streaming::parse(&json!({"Status": status}), &json!({"Path": path}))
    }

    #[test]
    fn reconnects_when_the_feed_moves_on() {
        let old = streaming(
            "Offline",
            "2026/2026-10-11_Singapore_Grand_Prix/2026-10-11_Race/",
        );
        let next = "2026/2026-10-25_United_States_Grand_Prix/2026-10-23_Practice_1/";
        assert!(!old.moved_on_from(&old));
        // New session, still offline, then live.
        let switched = streaming("Offline", next);
        assert!(switched.moved_on_from(&old));
        let live = streaming("Available", next);
        assert!(live.moved_on_from(&switched));
        assert!(!live.moved_on_from(&live));
        // Off the air after it.
        assert!(!switched.moved_on_from(&live));
    }
}
