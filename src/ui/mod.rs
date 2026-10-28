//! The app loop: one task owns all state and selects over feed messages,
//! keyboard input and a 100 ms redraw tick.

mod draw;
pub mod picker;
pub mod strip;
pub mod style;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use futures_util::FutureExt;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::awake::Awake;
use crate::circuit::{self, Circuit};
use crate::config::SignIn;
use crate::feed::replay::ReplayCmd;
use crate::feed::{ConnStatus, FeedEvent, ReplayStatus};
use crate::season::Season;
use crate::signin::Token;
use crate::state::State;
use crate::state::derive::Changed;
use crate::state::laps::TrendMode;
use strip::TrackLine;

const TICK: Duration = Duration::from_millis(100);
/// Car movement redraws at most 4 times a second.
const MAP_FRAME: Duration = Duration::from_millis(250);
const BLINK: Duration = Duration::from_millis(500);
/// How often to check the F1 TV sign-in.
const SIGN_IN_CHECK: Duration = Duration::from_secs(30);
pub const STALE_AFTER: Duration = Duration::from_secs(15);
/// Longest live delay.
pub const MAX_DELAY: Duration = Duration::from_secs(300);
/// Below this width race control becomes a full-screen view.
pub const WIDE: u16 = 131;

pub enum Source {
    /// Sending on it makes the live task reconnect.
    Live(mpsc::Sender<()>),
    Replay(mpsc::Sender<ReplayCmd>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Main,
    RaceControl,
}

pub struct App {
    state: State,
    source: Source,
    replay: Option<ReplayStatus>,
    conn: Option<ConnStatus>,
    last_msg: Option<Instant>,
    trend_mode: TrendMode,
    /// Hide DRS and blue-flag messages.
    rc_filter: bool,
    /// Show the lap line along the bottom.
    show_strip: bool,
    view: View,
    selected: Option<String>,
    track_line: Option<TrackLine>,
    circuit_req: Option<(i64, i32)>,
    /// Circuit key and year for which no geometry could be fetched.
    circuit_failed: Option<(i64, i32)>,
    circuit_tx: mpsc::Sender<((i64, i32), anyhow::Result<Circuit>)>,
    circuit_rx: mpsc::Receiver<((i64, i32), anyhow::Result<Circuit>)>,
    client: reqwest::Client,
    cache: PathBuf,
    notice: Option<String>,
    pending: Changed,
    dirty: bool,
    map_dirty: bool,
    last_draw: Instant,
    /// What the last frame showed for time-driven content, to redraw
    /// only when it changes.
    last_clock: (i64, bool, bool),
    started: Instant,
    width: u16,
    quit: bool,
    /// Live: data is held back this long, to stay in step with a video
    /// stream.
    delay: Duration,
    /// Live data waiting out the delay, with its arrival time.
    held: VecDeque<(Instant, FeedEvent)>,
    /// When the session start (lights out in a race) arrived, for `s`.
    start_arrived: Option<Instant>,
    /// Where to remember the delay.
    config: Option<PathBuf>,
    /// Live: the time on the video, being typed after `v`.
    video_time: Option<String>,
    /// The championship, while it loads.
    season_rx: Option<oneshot::Receiver<anyhow::Result<Season>>>,
    /// Keeps the display awake while there's something to watch.
    awake: Awake,
    /// The F1 TV token, and the config file to re-read it from.
    token: Option<String>,
    token_file: Option<PathBuf>,
    /// The sign-in as last checked, and when.
    sign_in: Option<SignIn>,
    sign_in_checked: Option<Instant>,
    /// The browser profile `l` signs in with.
    signin_profile: PathBuf,
    /// `l`: the sign-in in progress.
    signing_in: Option<JoinHandle<anyhow::Result<Token>>>,
}

pub struct AppOptions {
    pub source: Source,
    /// The championship around the session, loading.
    pub season: Option<oneshot::Receiver<anyhow::Result<Season>>>,
    pub client: reqwest::Client,
    pub cache: PathBuf,
    pub notice: Option<String>,
    /// Live delay to start with.
    pub delay: Duration,
    /// Config file to save the delay in when it changes.
    pub config: Option<PathBuf>,
    /// The F1 TV token, to warn as it nears expiry.
    pub token: Option<String>,
    /// The config file the token came from, re-read to notice a new
    /// sign-in, and where `l` saves one.
    pub token_file: Option<PathBuf>,
    /// The browser profile `l` signs in with.
    pub signin_profile: PathBuf,
}

impl App {
    pub fn new(o: AppOptions) -> Self {
        let (circuit_tx, circuit_rx) = mpsc::channel(4);
        Self {
            state: State::default(),
            source: o.source,
            replay: None,
            conn: None,
            last_msg: None,
            trend_mode: TrendMode::default(),
            rc_filter: false,
            show_strip: true,
            view: View::Main,
            selected: None,
            track_line: None,
            circuit_req: None,
            circuit_failed: None,
            circuit_tx,
            circuit_rx,
            client: o.client,
            cache: o.cache,
            notice: o.notice,
            pending: Changed::default(),
            dirty: true,
            map_dirty: false,
            last_draw: Instant::now(),
            last_clock: (0, false, false),
            started: Instant::now(),
            width: 0,
            quit: false,
            delay: o.delay.min(MAX_DELAY),
            held: VecDeque::new(),
            start_arrived: None,
            config: o.config,
            video_time: None,
            season_rx: o.season,
            awake: Awake::default(),
            token: o.token,
            token_file: o.token_file,
            sign_in: None,
            sign_in_checked: None,
            signin_profile: o.signin_profile,
            signing_in: None,
        }
    }

    pub async fn run(
        mut self,
        terminal: &mut DefaultTerminal,
        mut feed: mpsc::Receiver<FeedEvent>,
    ) -> anyhow::Result<()> {
        let (key_tx, mut keys) = mpsc::channel(64);
        std::thread::spawn(move || {
            while let Ok(ev) = event::read() {
                if key_tx.blocking_send(ev).is_err() {
                    break;
                }
            }
        });
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut feed_open = true;

        while !self.quit {
            tokio::select! {
                ev = feed.recv(), if feed_open => match ev {
                    Some(ev) => {
                        // Drain the burst; views are derived once per frame.
                        self.on_arrival(ev);
                        while let Ok(ev) = feed.try_recv() {
                            self.on_arrival(ev);
                        }
                        self.release_held();
                    }
                    None => {
                        feed_open = false;
                        self.notice = Some("feed ended".into());
                        self.dirty = true;
                    }
                },
                Some(ev) = keys.recv() => self.on_event(ev).await,
                Some((key, res)) = self.circuit_rx.recv() => self.on_circuit(key, res),
                _ = tick.tick() => {
                    self.release_held();
                    self.on_tick(terminal)?;
                }
            }
        }
        // Closes the sign-in window, if one is open.
        if let Some(h) = self.signing_in.take() {
            h.abort();
        }
        Ok(())
    }

    /// Live data is held for the delay; connection status shows at once.
    fn on_arrival(&mut self, ev: FeedEvent) {
        if !matches!(ev, FeedEvent::Message(_) | FeedEvent::Snapshot(_)) {
            return self.on_feed(ev);
        }
        let now = Instant::now();
        self.last_msg = Some(now);
        if let FeedEvent::Message(m) = &ev
            && m.topic == "SessionStatus"
            && m.data["Status"].as_str() == Some("Started")
        {
            self.start_arrived = Some(now);
        }
        self.held.push_back((now, ev));
    }

    /// Applies held data that has waited out the delay.
    fn release_held(&mut self) {
        while let Some((t, _)) = self.held.front() {
            if t.elapsed() < self.delay {
                break;
            }
            if let Some((_, ev)) = self.held.pop_front() {
                self.on_feed(ev);
            }
        }
    }

    fn set_delay(&mut self, d: Duration) {
        self.delay = d.min(MAX_DELAY);
        self.release_held();
        if let Some(path) = &self.config
            && let Err(e) = crate::config::save_delay(path, self.delay.as_secs())
        {
            tracing::warn!("saving delay: {e:#}");
        }
    }

    /// `s`, pressed at lights out on the video: the delay becomes the time
    /// since the session start arrived.
    fn sync_to_start(&mut self) {
        match self.start_arrived {
            Some(t) => {
                let d = Duration::from_secs(t.elapsed().as_secs_f64().round() as u64);
                self.set_delay(d);
                self.notice = Some(format!("delay synced to lights out: {}s", d.as_secs()));
            }
            None => {
                self.notice = Some("no session start seen yet: press s at lights out".into());
            }
        }
    }

    /// What `v` asks for: the race time in a race, else the time left.
    pub fn video_time_prompt(&self) -> &'static str {
        if self.state.views.session.race_length.is_some() {
            "race time on video"
        } else {
            "time left on video"
        }
    }

    /// Keys while typing the video time. Enter syncs, Esc gives up.
    fn on_video_time_key(&mut self, code: KeyCode) {
        let Some(text) = &mut self.video_time else {
            return;
        };
        match code {
            KeyCode::Char(c @ ('0'..='9' | ':' | '.')) if text.len() < 10 => text.push(c),
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Esc => self.video_time = None,
            KeyCode::Enter => {
                let text = self.video_time.take().unwrap_or_default();
                self.sync_to_video(&text);
            }
            _ => {}
        }
    }

    /// Sets the delay so the session clock reads `shown` (race time, or
    /// time left) now, as it does on the video.
    fn sync_to_video(&mut self, shown: &str) {
        let Some(shown) =
            crate::feed::parse_offset(shown).and_then(|d| TimeDelta::from_std(d).ok())
        else {
            self.notice = Some(format!("not a time: {shown:?} (try 5:04 or 1:05:04)"));
            return;
        };
        let Some(at) = self.state.views.session.when_clock_reads(shown) else {
            self.notice = Some("the session clock isn't running: can't sync to it".into());
            return;
        };
        let behind = (Utc::now() - at).num_milliseconds();
        if behind < 0 {
            self.notice = Some("the video is ahead of the data: delay set to 0".into());
            self.set_delay(Duration::ZERO);
            return;
        }
        let d = Duration::from_secs((behind as f64 / 1000.0).round() as u64);
        self.set_delay(d);
        self.notice = Some(format!(
            "delay synced to the video: {}s",
            self.delay.as_secs()
        ));
    }

    fn on_feed(&mut self, ev: FeedEvent) {
        match ev {
            FeedEvent::Message(m) => {
                let ch = self.state.apply(m);
                self.pending.union(ch);
            }
            FeedEvent::Snapshot(s) => {
                let ch = self.state.apply_snapshot(s);
                self.pending.union(ch);
            }
            FeedEvent::Reset => {
                self.state.reset();
                self.pending = Changed::all();
            }
            FeedEvent::Replay(st) => {
                self.replay = Some(st);
                self.dirty = true;
            }
            FeedEvent::Conn(c) => {
                self.conn = Some(c);
                self.dirty = true;
            }
        }
    }

    /// Re-derives the views touched since the last frame.
    fn flush(&mut self) {
        let ch = std::mem::take(&mut self.pending);
        self.state.derive(ch, self.trend_mode);
        self.dirty |= ch.any_text();
        self.map_dirty |= ch.map;
        if ch.session {
            self.request_circuit();
        }
    }

    fn request_circuit(&mut self) {
        let s = &self.state.views.session;
        let (Some(key), Some(year)) = (s.circuit_key, s.year) else {
            return;
        };
        if self.circuit_req == Some((key, year)) {
            return;
        }
        self.circuit_req = Some((key, year));
        let (client, cache, tx) = (
            self.client.clone(),
            self.cache.clone(),
            self.circuit_tx.clone(),
        );
        tokio::spawn(async move {
            let res = circuit::load(&client, &cache, key, year).await;
            let _ = tx.send(((key, year), res)).await;
        });
    }

    /// Takes the championship once it has loaded.
    fn check_season(&mut self) {
        let Some(rx) = &mut self.season_rx else {
            return;
        };
        let res = match rx.try_recv() {
            Err(oneshot::error::TryRecvError::Empty) => return,
            Err(oneshot::error::TryRecvError::Closed) => {
                self.season_rx = None;
                return;
            }
            Ok(res) => res,
        };
        self.season_rx = None;
        match res {
            Ok(s) => {
                tracing::info!("championship: {} round {}", s.year, s.round);
                self.state.season = Some(s);
                self.pending.union(Changed {
                    tower: true,
                    ..Changed::default()
                });
            }
            Err(e) => {
                tracing::warn!("championship unavailable: {e:#}");
                self.notice = Some(format!("no championship data: {e:#}"));
                self.dirty = true;
            }
        }
    }

    fn on_circuit(&mut self, key: (i64, i32), res: anyhow::Result<Circuit>) {
        match res {
            Ok(c) => {
                self.track_line = Some(TrackLine::new(&c));
                self.circuit_failed = None;
            }
            Err(e) => {
                tracing::warn!("circuit geometry unavailable: {e:#}");
                self.circuit_failed = Some(key);
            }
        }
        self.dirty = true;
    }

    /// Warns when the F1 TV sign-in is about to expire or has, and picks up
    /// a new one made with `pitwall login` meanwhile.
    fn check_sign_in(&mut self, now: DateTime<Utc>) {
        if self
            .sign_in_checked
            .is_some_and(|t| t.elapsed() < SIGN_IN_CHECK)
        {
            return;
        }
        self.sign_in_checked = Some(Instant::now());
        if let Some(path) = &self.token_file
            && let Ok(cfg) = crate::config::load(path)
        {
            self.token = cfg.token;
        }
        let state = SignIn::of(self.token.as_deref(), now);
        let was = self.sign_in.replace(state);
        if was.is_some_and(|w| std::mem::discriminant(&w) == std::mem::discriminant(&state)) {
            return;
        }
        if let Some(n) = state.notice() {
            self.notice = Some(n);
            self.dirty = true;
        } else if state == SignIn::Valid
            && let Some(was) = was
        {
            // Signed in. A live feed connected without a usable token
            // reconnects to get positions and telemetry.
            self.notice = Some("signed in to F1 TV".into());
            if (was.is_dead() || was == SignIn::Missing)
                && let Source::Live(reset) = &self.source
            {
                let _ = reset.try_send(());
                self.notice = Some("signed in to F1 TV: reconnecting".into());
            }
            self.dirty = true;
        }
    }

    /// `l`: signs in to F1 TV in a browser window, in the background.
    fn start_sign_in(&mut self) {
        self.dirty = true;
        if self.signing_in.is_some() {
            self.notice = Some("finish signing in in the browser window".into());
            return;
        }
        if self.token_file.is_none() {
            // The token came from --token or PITWALL_TOKEN, or isn't needed.
            self.notice = Some("can't sign in here: use `pitwall login`".into());
            return;
        }
        let profile = self.signin_profile.clone();
        self.signing_in = Some(tokio::spawn(async move {
            crate::signin::sign_in(&profile).await
        }));
        self.notice = Some("sign in to F1 TV in the browser window (close it to cancel)".into());
    }

    /// Saves the token once the sign-in started with `l` is done.
    fn check_signing_in(&mut self, now: DateTime<Utc>) {
        let Some(res) = self.signing_in.as_mut().and_then(|h| h.now_or_never()) else {
            return;
        };
        self.signing_in = None;
        self.dirty = true;
        let saved = res.map_err(anyhow::Error::from).and_then(|r| {
            let t = r?;
            if let Some(path) = &self.token_file {
                crate::config::save_token(path, &t.token)?;
            }
            Ok(t)
        });
        match saved {
            Ok(t) => {
                self.token = Some(t.token);
                self.notice = Some("signed in to F1 TV".into());
                // Pick it up now, reconnecting if the feed needs it.
                self.sign_in_checked = None;
                self.check_sign_in(now);
            }
            Err(e) => self.notice = Some(format!("F1 TV sign-in: {e:#}")),
        }
    }

    fn on_tick(&mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        self.check_season();
        self.check_signing_in(Utc::now());
        self.check_sign_in(Utc::now());
        self.awake.set(self.watching());
        self.flush();
        // Fallback outline from position samples.
        if self.track_line.is_none()
            && let Some(pts) = self.state.outline.done.take()
        {
            let c = Circuit::from_samples(&pts);
            if let Some((key, year)) = self.circuit_failed {
                circuit::save_fallback(&self.cache, key, year, &c);
            }
            self.track_line = Some(TrackLine::new(&c));
            self.dirty = true;
        }

        let blink = (self.started.elapsed().as_millis() / BLINK.as_millis()) % 2 == 1;
        let clock = (
            self.now().timestamp(),
            blink && self.blinking(),
            self.is_stale(),
        );
        let since = self.last_draw.elapsed();
        let redraw = self.dirty
            || clock != self.last_clock
            || (self.map_dirty && since >= MAP_FRAME && self.show_strip);
        if !redraw {
            return Ok(());
        }
        let mut width = self.width;
        terminal.draw(|f| {
            width = f.area().width;
            draw::draw(f, self, blink);
        })?;
        self.width = width;
        self.dirty = false;
        self.map_dirty = false;
        self.last_draw = Instant::now();
        self.last_clock = clock;
        Ok(())
    }

    /// Whether anything on screen blinks right now.
    fn blinking(&self) -> bool {
        let v = &self.state.views;
        v.track.flashes()
            || v.sector_flags
                .values()
                .any(|f| *f == crate::state::derive::SectorFlag::DoubleYellow)
            || (self.show_strip && !v.stopped.is_empty())
            || self
                .state
                .title_secured()
                .is_some_and(|(_, since)| self.now() - since < draw::TITLE_FLASH)
            || self
                .state
                .penalised()
                .keys()
                .any(|num| draw::penalty_flashing(self, num))
    }

    /// Live while data is coming in; a replay while it plays.
    fn watching(&self) -> bool {
        match (&self.source, &self.replay) {
            (Source::Live(_), _) => !self.is_stale(),
            (Source::Replay(_), Some(st)) => !st.paused && st.current() < st.end,
            (Source::Replay(_), None) => false,
        }
    }

    pub fn is_stale(&self) -> bool {
        matches!(self.source, Source::Live(_))
            && self.last_msg.is_none_or(|t| t.elapsed() > STALE_AFTER)
    }

    pub fn delay(&self) -> Option<Duration> {
        matches!(self.source, Source::Live(_)).then_some(self.delay)
    }

    /// Current feed time: wall clock (less the delay) live, playback
    /// position in replay.
    pub fn now(&self) -> DateTime<Utc> {
        match &self.replay {
            Some(st) => self
                .state
                .utc_at(st.current())
                .or(self.state.feed_utc)
                .unwrap_or_else(Utc::now),
            None => Utc::now() - TimeDelta::from_std(self.delay).unwrap_or_default(),
        }
    }

    async fn on_event(&mut self, ev: Event) {
        let Event::Key(k) = ev else {
            if matches!(ev, Event::Resize(..)) {
                self.dirty = true;
            }
            return;
        };
        if k.kind != KeyEventKind::Press {
            return;
        }
        if self.video_time.is_some() && !k.modifiers.contains(KeyModifiers::CONTROL) {
            self.on_video_time_key(k.code);
            self.dirty = true;
            return;
        }
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Esc => {
                if self.view == View::Main {
                    self.selected = None;
                }
                self.view = View::Main;
            }
            KeyCode::Char('t') => {
                self.trend_mode = match self.trend_mode {
                    TrendMode::Avg3 => TrendMode::Last,
                    TrendMode::Last => TrendMode::Avg3,
                };
                self.pending.tower = true;
            }
            KeyCode::Char('f') => self.rc_filter = !self.rc_filter,
            KeyCode::Char('x') => self.reset(),
            KeyCode::Char(c @ ('s' | 'v' | '+' | '=' | '-' | '_'))
                if matches!(self.source, Source::Live(_)) =>
            {
                let secs = self.delay.as_secs();
                match c {
                    's' => self.sync_to_start(),
                    'v' => self.video_time = Some(String::new()),
                    // `=` is + without shift.
                    '+' | '=' => self.set_delay(Duration::from_secs(secs + 1)),
                    _ => self.set_delay(Duration::from_secs(secs.saturating_sub(1))),
                }
            }
            KeyCode::Char('m') => self.show_strip = !self.show_strip,
            KeyCode::Char('l') => self.start_sign_in(),
            KeyCode::Char('r') => {
                self.view = if self.view == View::RaceControl {
                    View::Main
                } else {
                    View::RaceControl
                };
            }
            code => {
                if let Source::Replay(tx) = &self.source {
                    let cmd = match code {
                        KeyCode::Char(' ') => Some(ReplayCmd::TogglePause),
                        KeyCode::Char('+' | '=') => Some(ReplayCmd::Faster),
                        KeyCode::Char('-' | '_') => Some(ReplayCmd::Slower),
                        KeyCode::Left => Some(ReplayCmd::Seek(-10)),
                        KeyCode::Right => Some(ReplayCmd::Seek(10)),
                        KeyCode::PageUp => Some(ReplayCmd::Seek(-30)),
                        KeyCode::PageDown => Some(ReplayCmd::Seek(30)),
                        _ => None,
                    };
                    if let Some(cmd) = cmd {
                        let _ = tx.send(cmd).await;
                    }
                }
            }
        }
        self.dirty = true;
    }

    /// Live only: forget everything and reconnect for a fresh snapshot.
    fn reset(&mut self) {
        let Source::Live(reset) = &self.source else {
            return;
        };
        let _ = reset.try_send(());
        self.held.clear();
        self.state = State::default();
        self.pending = Changed::all();
        self.last_msg = None;
        self.conn = Some(ConnStatus::Connecting);
    }

    fn move_selection(&mut self, by: isize) {
        let rows = &self.state.views.tower;
        if rows.is_empty() {
            return;
        }
        let cur = self
            .selected
            .as_ref()
            .and_then(|s| rows.iter().position(|r| &r.num == s));
        let next = match cur {
            None => 0,
            Some(i) => (i as isize + by).clamp(0, rows.len() as isize - 1) as usize,
        };
        self.selected = Some(rows[next].num.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::FeedMessage;
    use serde_json::json;

    fn live_app(delay: u64) -> App {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (reset, _) = mpsc::channel(1);
        App::new(AppOptions {
            source: Source::Live(reset),
            season: None,
            client: reqwest::Client::new(),
            cache: PathBuf::new(),
            notice: None,
            delay: Duration::from_secs(delay),
            config: None,
            token: None,
            token_file: None,
            signin_profile: PathBuf::new(),
        })
    }

    fn msg(topic: &str, data: serde_json::Value) -> FeedEvent {
        FeedEvent::Message(FeedMessage {
            topic: topic.into(),
            data,
            offset: Duration::ZERO,
            utc: None,
        })
    }

    fn jwt(exp: i64) -> String {
        use base64::Engine;
        let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.sig",
            e.encode(r#"{"alg":"none"}"#),
            e.encode(format!(r#"{{"exp":{exp}}}"#))
        )
    }

    #[tokio::test]
    async fn l_saves_the_new_token_and_reconnects() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        let now = Utc::now();
        crate::config::save_token(&file, &jwt(now.timestamp() - 60)).unwrap();

        let (reset, mut resets) = mpsc::channel(1);
        let mut app = live_app(0);
        app.source = Source::Live(reset);
        app.token_file = Some(file.clone());
        app.check_sign_in(now);
        assert_eq!(
            app.notice.as_deref(),
            Some("F1 TV sign-in expired: press l to sign in")
        );

        // What the browser window hands back once signed in.
        let fresh = jwt(now.timestamp() + 4 * 86400);
        let token = Token {
            token: fresh.clone(),
            expires: now + TimeDelta::days(4),
        };
        let h = tokio::spawn(async move { Ok(token) });
        while !h.is_finished() {
            tokio::task::yield_now().await;
        }
        app.signing_in = Some(h);
        app.check_signing_in(now);

        assert!(app.signing_in.is_none());
        assert_eq!(
            crate::config::load(&file).unwrap().token.as_deref(),
            Some(fresh.as_str())
        );
        assert_eq!(
            app.notice.as_deref(),
            Some("signed in to F1 TV: reconnecting")
        );
        assert!(resets.try_recv().is_ok());
    }

    #[test]
    fn warns_as_the_sign_in_expires_and_picks_up_a_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        let t0 = 1_800_000_000;
        let at = |s: i64| DateTime::from_timestamp(t0 + s, 0).unwrap();
        crate::config::save_token(&file, &jwt(t0 + 5 * 3600)).unwrap();

        let (reset, mut resets) = mpsc::channel(1);
        let mut app = live_app(0);
        app.source = Source::Live(reset);
        app.token_file = Some(file.clone());
        let check = |app: &mut App, s: i64| {
            app.sign_in_checked = None;
            app.check_sign_in(at(s));
        };

        check(&mut app, 0);
        assert_eq!(app.notice, None);
        check(&mut app, 3600 + 60);
        assert_eq!(
            app.notice.as_deref(),
            Some("F1 TV sign-in expires in 3h 59m: press l to sign in")
        );
        // Only when the state changes, so other notices aren't buried.
        app.notice = Some("delay synced".into());
        check(&mut app, 2 * 3600);
        assert_eq!(app.notice.as_deref(), Some("delay synced"));
        check(&mut app, 5 * 3600);
        assert_eq!(
            app.notice.as_deref(),
            Some("F1 TV sign-in expired: press l to sign in")
        );
        assert!(resets.try_recv().is_err());

        // `pitwall login` in another terminal: the feed reconnects with it.
        crate::config::save_token(&file, &jwt(t0 + 4 * 86400)).unwrap();
        check(&mut app, 5 * 3600 + 30);
        assert_eq!(
            app.notice.as_deref(),
            Some("signed in to F1 TV: reconnecting")
        );
        assert!(resets.try_recv().is_ok());
    }

    #[test]
    fn delay_holds_data_back() {
        let mut app = live_app(60);
        app.on_arrival(msg("LapCount", json!({"CurrentLap": 7})));
        app.on_arrival(FeedEvent::Conn(ConnStatus::Connected));
        app.release_held();
        // Connection status shows at once, data waits.
        assert!(matches!(app.conn, Some(ConnStatus::Connected)));
        assert!(app.state.topics.get("LapCount").is_none());
        assert_eq!(app.held.len(), 1);
        assert!(app.last_msg.is_some());

        app.set_delay(Duration::ZERO);
        assert!(app.held.is_empty());
        assert_eq!(app.state.topics["LapCount"]["CurrentLap"], 7);
    }

    #[test]
    fn sync_to_session_start() {
        let mut app = live_app(0);
        app.sync_to_start();
        assert_eq!(app.delay, Duration::ZERO);
        assert!(app.notice.as_deref().unwrap().contains("no session start"));

        app.on_arrival(msg("SessionStatus", json!({"Status": "Started"})));
        // Pretend the start arrived 31.6 s before the key press.
        app.start_arrived = Instant::now().checked_sub(Duration::from_millis(31_600));
        app.sync_to_start();
        assert_eq!(app.delay, Duration::from_secs(32));
        assert!(app.now() < Utc::now() - TimeDelta::seconds(31));

        app.set_delay(Duration::from_secs(1000));
        assert_eq!(app.delay, MAX_DELAY);
    }

    #[test]
    fn stopped_car_in_qualifying() {
        let mut app = live_app(0);
        let feed = |app: &mut App, topic: &str, data| {
            let FeedEvent::Message(m) = msg(topic, data) else {
                unreachable!()
            };
            let ch = app.state.apply(m);
            app.state.derive(ch, app.trend_mode);
        };
        let line = |stopped: bool| {
            json!({
                "Position": "1", "NumberOfLaps": 3, "Stopped": stopped,
                "BestLapTimes": [{"Value": "1:22.000"}],
                "Sectors": [{"Value": "27.0", "Segments": [{"Status": 2049}]}],
            })
        };
        for (topic, data) in [
            (
                "SessionInfo",
                json!({"Type": "Qualifying", "Name": "Qualifying"}),
            ),
            ("SessionStatus", json!({"Status": "Started"})),
            ("DriverList", json!({"4": {"Tla": "NOR"}})),
            (
                "TimingData",
                json!({"SessionPart": 1, "NoEntries": [22, 16, 10], "Lines": {"4": line(false)}}),
            ),
            // Over the line after an out lap: a push lap.
            ("TimingData", json!({"Lines": {"4": {"NumberOfLaps": 4}}})),
        ] {
            feed(&mut app, topic, data);
        }
        let render = |app: &App| {
            let mut t =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 40)).unwrap();
            t.draw(|f| draw::draw(f, app, false)).unwrap();
            let buf = t.backend().buffer().clone();
            let text: Vec<String> = (0..40)
                .map(|y| (0..140).map(|x| buf[(x, y)].symbol()).collect())
                .collect();
            (text, buf)
        };
        let (text, _) = render(&app);
        assert!(text.iter().any(|l| l.contains("PUSH")), "{text:#?}");
        let push = text.iter().position(|l| l.contains("push lap")).unwrap();
        assert!(text[push + 1..].iter().any(|l| l.contains("NOR")));

        feed(
            &mut app,
            "TimingData",
            json!({"Lines": {"4": {"Stopped": true}}}),
        );
        let (text, buf) = render(&app);
        let row = text
            .iter()
            .position(|l| l.contains("STOPPED"))
            .expect("marked stopped");
        let x = text[row].find("NOR").unwrap() as u16;
        assert_eq!(buf[(x, row as u16)].bg, style::stopped_row());
        // Only the push lap box: the stopped list beside it names NOR too.
        let push = text.iter().position(|l| l.contains("push lap")).unwrap();
        let box_w = text[push].chars().position(|c| c == '╮').unwrap();
        assert!(
            !text[push + 1..].iter().any(|l| l
                .chars()
                .take(box_w)
                .collect::<String>()
                .contains("NOR"))
        );
    }

    #[test]
    fn sync_to_video_time() {
        let feed = |app: &mut App, topic: &str, data| {
            let FeedEvent::Message(m) = msg(topic, data) else {
                unreachable!()
            };
            let ch = app.state.apply(m);
            app.state.derive(ch, app.trend_mode);
        };
        // The race clock started 400 s ago by the data.
        let clock = |app: &mut App, remaining: &str, ago: i64| {
            let utc = (Utc::now() - TimeDelta::seconds(ago)).to_rfc3339();
            feed(
                app,
                "ExtrapolatedClock",
                json!({"Utc": utc, "Remaining": remaining, "Extrapolating": true}),
            );
        };
        let mut app = live_app(0);
        feed(
            &mut app,
            "SessionInfo",
            json!({"Type": "Race", "Name": "Race"}),
        );
        feed(&mut app, "SessionStatus", json!({"Status": "Started"}));
        clock(&mut app, "02:00:00", 400);
        assert_eq!(app.video_time_prompt(), "race time on video");
        let shown = app.state.views.session.race_time_at(app.now()).unwrap();
        assert_eq!(shown.num_seconds(), 400);

        // The video shows 5:04: it's 96 s behind.
        app.video_time = Some(String::new());
        for c in "5:04".chars() {
            app.on_video_time_key(KeyCode::Char(c));
        }
        app.on_video_time_key(KeyCode::Enter);
        assert!(app.video_time.is_none());
        assert_eq!(app.delay, Duration::from_secs(96));
        let shown = app.state.views.session.race_time_at(app.now()).unwrap();
        assert_eq!(shown.num_seconds(), 304);

        // Qualifying: the time left, here 17:30 on the video.
        let mut app = live_app(0);
        feed(
            &mut app,
            "SessionInfo",
            json!({"Type": "Qualifying", "Name": "Qualifying"}),
        );
        clock(&mut app, "00:18:00", 0);
        assert_eq!(app.video_time_prompt(), "time left on video");
        app.sync_to_video("17:30");
        assert_eq!(app.delay, Duration::ZERO);
        assert!(app.notice.as_deref().unwrap().contains("ahead"));
        app.sync_to_video("18:20");
        assert_eq!(app.delay, Duration::from_secs(20));
        app.sync_to_video("soon");
        assert!(app.notice.as_deref().unwrap().contains("not a time"));
    }
}
