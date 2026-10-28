//! pitwall: F1 live timing in the terminal.

mod awake;
mod circuit;
mod config;
mod feed;
mod login;
mod season;
mod signin;
mod state;
mod ui;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use chrono::{Datelike, NaiveDate};
use clap::{Parser, Subcommand};
use tokio::sync::{mpsc, oneshot};

use config::{Config, Paths, SignIn};
use feed::archive;
use feed::live::{self, LiveOptions};
use feed::record::Recorder;
use feed::replay::Replay;
use feed::stream::SessionFiles;
use feed::{FeedEvent, fmt_offset};
use ui::{App, AppOptions, Source};

#[derive(Parser)]
#[command(name = "pitwall", version, about = "F1 live timing in your terminal")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Config file (default: the platform config dir).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Skip car positions and telemetry (Position.z, CarData.z); the lap
    /// line then places cars by timing mini-sector.
    #[arg(long, global = true)]
    no_map: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Follow the live session (the default).
    Live {
        /// Also record every message, optionally into DIR.
        #[arg(long, value_name = "DIR")]
        record: Option<Option<PathBuf>>,
        /// F1 TV subscription token (overrides the config file).
        #[arg(long, env = "PITWALL_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// Replay an archived session, e.g. `replay 2025 singapore race`.
    ///
    /// Leave out the session to choose it from a list, or the meeting too
    /// to browse the season (default: this year).
    Replay {
        year: Option<i32>,
        /// Meeting name, location or country, e.g. `monza` or `italy`.
        meeting: Option<String>,
        /// Session: race, quali, sprint, sq, fp1, fp2, fp3.
        session: Option<String>,
        /// Replay a directory recorded with `--record` instead.
        #[arg(long, conflicts_with_all = ["year", "meeting", "session"])]
        dir: Option<PathBuf>,
        /// Playback speed (1 to 60).
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
        /// Start offset as HH:MM:SS (default: one minute before the start).
        #[arg(long)]
        start: Option<String>,
        /// Print the messages in order instead of opening the UI.
        #[arg(long)]
        dump: bool,
    },
    /// Record the live session to disk without the UI.
    Record {
        /// Output directory.
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, env = "PITWALL_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// List a season's meetings and sessions.
    List {
        year: i32,
        /// Only meetings matching this.
        meeting: Option<String>,
    },
    /// Sign in to F1 TV and save your token.
    ///
    /// Opens F1's sign-in page in a Chrome (or Brave, Edge, Chromium) window
    /// of its own and saves the token once you've signed in.
    Login {
        /// Instead, take the token from a browser you're already signed in
        /// with: every supported browser, or just NAME (e.g. `chrome`,
        /// `firefox`). Reads only the `login-session` cookie for
        /// formula1.com; macOS asks to allow Keychain access for Chromium
        /// browsers.
        #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "all")]
        browser: Option<String>,
        /// With --browser: show where cookies were looked for and the names
        /// (not values) of the formula1.com cookies found.
        #[arg(long)]
        debug: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    let cfg_path = cli
        .config
        .clone()
        .unwrap_or_else(config::default_config_file);
    if let Some(Cmd::Login { browser, debug }) = &cli.cmd {
        let profile = Paths::new(&Config::default()).signin_profile;
        return login::run(browser.as_deref(), &cfg_path, &profile, *debug);
    }
    let cfg = config::load(&cfg_path)?;
    let paths = Paths::new(&cfg);
    let _log = init_logging(&paths.logs)?;
    tracing::info!("config {}", cfg_path.display());

    // One thread: the feed tasks and the app loop never wake each other
    // across threads, which keeps idle CPU low.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(cli, cfg, paths, cfg_path))
}

async fn run(cli: Cli, cfg: Config, paths: Paths, cfg_path: PathBuf) -> anyhow::Result<()> {
    let map = cfg.map && !cli.no_map;
    let client = archive::http_client()?;
    match cli.cmd.unwrap_or(Cmd::Live {
        record: None,
        token: None,
    }) {
        Cmd::Live { record, token } => {
            let (token, token_file) = token_source(token, &cfg, &cfg_path);
            // The app warns about an expiring or expired sign-in itself.
            let notice = (token.is_none()).then(|| {
                "no F1 TV token: car positions by mini-sector only (l to sign in)".to_owned()
            });
            let recorder = match record {
                Some(dir) => Some(Recorder::new(
                    &dir.unwrap_or_else(|| new_recording_dir(&paths)),
                )?),
                None => None,
            };
            let notice = match (&recorder, notice) {
                (Some(r), None) => Some(format!("recording to {}", r.dir().display())),
                (_, n) => n,
            };
            let opts = LiveOptions {
                token: token.clone(),
                token_file: token_file.clone(),
                token_in_query: cfg.token_in_query,
                topics: feed::topics(map),
                recorder,
            };
            let (tx, rx) = mpsc::channel(4096);
            let (reset_tx, reset_rx) = mpsc::channel(1);
            tokio::spawn(live::run(opts, tx, reset_rx));
            let season = load_season(&client, &paths.cache, chrono::Utc::now().date_naive());
            run_ui(
                AppOptions {
                    source: Source::Live(reset_tx),
                    season: Some(season),
                    client,
                    cache: paths.cache,
                    notice,
                    delay: Duration::from_secs(cfg.delay),
                    config: Some(cfg_path),
                    token,
                    token_file,
                    signin_profile: paths.signin_profile,
                },
                rx,
            )
            .await
        }
        Cmd::Replay {
            year,
            meeting,
            session,
            dir,
            speed,
            start,
            dump,
        } => {
            let topics = feed::topics(map);
            let token = cfg.token.as_deref();
            // A recorded directory needs no sign-in.
            let archived = dir.is_none();
            if archived {
                report_sign_in(token);
            }
            let files = match dir {
                Some(dir) => SessionFiles::open_dir(&dir, &topics)?,
                None => {
                    let year = year.unwrap_or_else(|| chrono::Local::now().year());
                    let index = archive::fetch_index(&client, token, year, &paths.cache).await?;
                    let (m, s) = match (meeting, session) {
                        (Some(meeting), Some(session)) => {
                            archive::find_session(&index, &meeting, &session)?
                        }
                        // Browse: the season, or the meeting's sessions.
                        (meeting, _) => {
                            let start = meeting
                                .map(|q| archive::find_meeting(&index, &q))
                                .transpose()?;
                            let mut terminal = ratatui::init();
                            let choice = ui::picker::pick(&mut terminal, &index, start);
                            ratatui::restore();
                            let Some((m, s)) = choice? else {
                                return Ok(());
                            };
                            let m = &index.meetings[m];
                            (m, &m.sessions[s])
                        }
                    };
                    let path = s.path.as_deref().with_context(|| {
                        format!("{} {} has no archive path yet", m.name, s.name)
                    })?;
                    eprintln!("Loading {} · {} ({path})…", m.name, s.name);
                    archive::fetch_session(&client, token, path, &topics, &paths.cache).await?
                }
            };
            let start = match start {
                Some(s) => feed::parse_offset(&s).with_context(|| format!("bad --start {s:?}"))?,
                None => files
                    .start()
                    .map(|t| t.saturating_sub(Duration::from_secs(60)))
                    .unwrap_or_default(),
            };
            if dump {
                return dump_messages(&files, start);
            }
            let season = session_date(&files).map(|d| load_season(&client, &paths.cache, d));
            let (tx, rx) = mpsc::channel(4096);
            let (cmd_tx, cmd_rx) = mpsc::channel(16);
            let replay = Replay::new(files, tx, speed.clamp(1.0, 60.0));
            tokio::spawn(replay.run(start, cmd_rx));
            run_ui(
                AppOptions {
                    source: Source::Replay(cmd_tx),
                    season,
                    client,
                    cache: paths.cache,
                    notice: None,
                    delay: Duration::ZERO,
                    config: None,
                    token: cfg.token.clone().filter(|_| archived),
                    token_file: archived.then_some(cfg_path),
                    signin_profile: paths.signin_profile,
                },
                rx,
            )
            .await
        }
        Cmd::Record { out, token } => {
            let (token, token_file) = token_source(token, &cfg, &cfg_path);
            report_sign_in(token.as_deref());
            let dir = out.unwrap_or_else(|| new_recording_dir(&paths));
            record_headless(token, token_file, cfg.token_in_query, &dir).await
        }
        Cmd::Login { .. } => unreachable!("handled before the runtime starts"),
        Cmd::List { year, meeting } => {
            report_sign_in(cfg.token.as_deref());
            let index =
                archive::fetch_index(&client, cfg.token.as_deref(), year, &paths.cache).await?;
            let q = meeting.map(|m| m.to_lowercase());
            for m in &index.meetings {
                let hay = format!(
                    "{} {} {}",
                    m.name,
                    m.location,
                    m.country.as_ref().map_or("", |c| c.name.as_str())
                )
                .to_lowercase();
                if q.as_ref().is_some_and(|q| !hay.contains(q)) {
                    continue;
                }
                println!("{} ({})", m.name, m.location);
                for s in &m.sessions {
                    let avail = if s.path.is_some() {
                        ""
                    } else {
                        "  (not archived)"
                    };
                    println!(
                        "    {:<20} {}{avail}",
                        s.name,
                        s.start_date.get(..16).unwrap_or(&s.start_date)
                    );
                }
            }
            Ok(())
        }
    }
}

async fn run_ui(opts: AppOptions, rx: mpsc::Receiver<FeedEvent>) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let res = App::new(opts).run(&mut terminal, rx).await;
    ratatui::restore();
    res
}

/// Loads the championship around a session's day in the background, once.
fn load_season(
    client: &reqwest::Client,
    cache: &Path,
    date: NaiveDate,
) -> oneshot::Receiver<anyhow::Result<season::Season>> {
    let (tx, rx) = oneshot::channel();
    let (client, cache) = (client.clone(), cache.to_owned());
    tokio::spawn(async move {
        let _ = tx.send(season::load(&client, &cache, date).await);
    });
    rx
}

/// The day a recorded or archived session started, from its SessionInfo.
fn session_date(files: &SessionFiles) -> Option<NaiveDate> {
    let info = files.topic("SessionInfo").into_iter().next()?;
    let start = info.data["StartDate"].as_str()?;
    NaiveDate::parse_from_str(start.get(..10)?, "%Y-%m-%d").ok()
}

/// The token from `--token` / `PITWALL_TOKEN`, or else the config's,
/// with the config file to re-read it from in the second case.
fn token_source(
    flag: Option<String>,
    cfg: &Config,
    cfg_path: &Path,
) -> (Option<String>, Option<PathBuf>) {
    match flag.as_deref().and_then(config::normalise_token) {
        Some(t) => (Some(t), None),
        None => (cfg.token.clone(), Some(cfg_path.to_owned())),
    }
}

/// Says on stderr if the F1 TV sign-in is missing, expiring or expired.
fn report_sign_in(token: Option<&str>) {
    if let Some(m) = SignIn::of(token, chrono::Utc::now()).message() {
        eprintln!("{m}");
    }
}

fn new_recording_dir(paths: &Paths) -> PathBuf {
    paths
        .recordings
        .join(chrono::Local::now().format("%Y-%m-%d_%H%M%S").to_string())
}

fn dump_messages(files: &SessionFiles, start: Duration) -> anyhow::Result<()> {
    let mut out = std::io::stdout().lock();
    for m in files.open()?.filter(|m| m.offset >= start) {
        let mut data = m.data.to_string();
        if data.len() > 160 {
            let cut = (0..=160)
                .rev()
                .find(|&i| data.is_char_boundary(i))
                .unwrap_or(0);
            data.truncate(cut);
            data.push('…');
        }
        if writeln!(out, "{} {:<20} {data}", fmt_offset(m.offset), m.topic).is_err() {
            break; // stdout closed, e.g. piped into `head`
        }
    }
    Ok(())
}

async fn record_headless(
    token: Option<String>,
    token_file: Option<PathBuf>,
    token_in_query: bool,
    dir: &Path,
) -> anyhow::Result<()> {
    let recorder = Recorder::new(dir)?;
    eprintln!("Recording to {} (Ctrl-C to stop)", dir.display());
    let opts = LiveOptions {
        token,
        token_file,
        token_in_query,
        topics: feed::topics(true),
        recorder: Some(recorder),
    };
    let (tx, mut rx) = mpsc::channel(4096);
    let (_reset_tx, reset_rx) = mpsc::channel(1);
    tokio::spawn(live::run(opts, tx, reset_rx));
    let mut count = 0u64;
    let mut report = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(FeedEvent::Message(_)) => count += 1,
                Some(FeedEvent::Snapshot(s)) => eprintln!("snapshot: {} topics", s.len()),
                Some(FeedEvent::Conn(c)) => eprintln!("{c:?}"),
                Some(_) => {}
                None => break,
            },
            _ = report.tick() => eprintln!("{count} messages recorded"),
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    eprintln!("Stopped. {count} messages in {}", dir.display());
    Ok(())
}

fn init_logging(dir: &Path) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let appender = tracing_appender::rolling::daily(dir, "pitwall.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_env("PITWALL_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();
    Ok(guard)
}
