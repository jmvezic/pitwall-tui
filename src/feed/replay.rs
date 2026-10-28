//! Plays a session from its files on disk on a timer, with pause, speed
//! and seek. Only the read position is held, not the messages.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::time::sleep_until;

use super::stream::{Reader, SessionFiles};
use super::{FeedEvent, ReplayStatus};

pub const SPEEDS: &[f64] = &[1.0, 2.0, 4.0, 8.0, 15.0, 30.0, 60.0];

/// While fast-forwarding, `.z` samples older than this before the target
/// are skipped. Long enough to see the field line up on the grid before a
/// start, and to time a stop past [`crate::state::RETIRE_AFTER`].
const Z_KEEP: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, Copy)]
pub enum ReplayCmd {
    TogglePause,
    Faster,
    Slower,
    /// Relative seek in seconds.
    Seek(i64),
}

pub struct Replay {
    files: SessionFiles,
    /// The next message to play; `None` until the first seek.
    reader: Option<Reader>,
    tx: mpsc::Sender<FeedEvent>,
    /// Playback position at `anchor`.
    pos: Duration,
    anchor: Instant,
    speed_idx: usize,
    paused: bool,
    end: Duration,
}

impl Replay {
    pub fn new(files: SessionFiles, tx: mpsc::Sender<FeedEvent>, speed: f64) -> Self {
        let end = files.end;
        let speed_idx = SPEEDS
            .iter()
            .position(|&s| s >= speed)
            .unwrap_or(SPEEDS.len() - 1);
        Self {
            files,
            reader: None,
            tx,
            pos: Duration::ZERO,
            anchor: Instant::now(),
            speed_idx,
            paused: false,
            end,
        }
    }

    fn speed(&self) -> f64 {
        SPEEDS[self.speed_idx]
    }

    fn current(&self) -> Duration {
        if self.paused {
            self.pos
        } else {
            self.pos + self.anchor.elapsed().mul_f64(self.speed())
        }
    }

    fn set_pos(&mut self, pos: Duration) {
        self.pos = pos.min(self.end);
        self.anchor = Instant::now();
    }

    /// Runs until the receiving side goes away.
    pub async fn run(mut self, start: Duration, mut cmds: mpsc::Receiver<ReplayCmd>) {
        if self.seek_to(start).await.is_err() {
            return;
        }
        loop {
            let now = self.current();
            let Some(reader) = &mut self.reader else {
                return;
            };
            while reader.peek().is_some_and(|t| t <= now) {
                let Some(m) = reader.pop().and_then(|l| l.parse()) else {
                    continue;
                };
                if self.tx.send(FeedEvent::Message(m)).await.is_err() {
                    return;
                }
            }
            let due = (!self.paused)
                .then(|| reader.peek())
                .flatten()
                .map(|t| self.anchor + (t.saturating_sub(self.pos)).div_f64(self.speed()));
            let cmd = match due {
                Some(due) => tokio::select! {
                    _ = sleep_until(due.into()) => continue,
                    c = cmds.recv() => c,
                },
                None => cmds.recv().await,
            };
            let Some(cmd) = cmd else { return };
            let now = self.current();
            let res = match cmd {
                ReplayCmd::TogglePause => {
                    self.paused = !self.paused;
                    self.set_pos(now);
                    self.status().await
                }
                ReplayCmd::Faster | ReplayCmd::Slower => {
                    self.speed_idx = match cmd {
                        ReplayCmd::Faster => (self.speed_idx + 1).min(SPEEDS.len() - 1),
                        _ => self.speed_idx.saturating_sub(1),
                    };
                    self.set_pos(now);
                    self.status().await
                }
                ReplayCmd::Seek(secs) => {
                    let target = if secs >= 0 {
                        now + Duration::from_secs(secs as u64)
                    } else {
                        now.saturating_sub(Duration::from_secs(secs.unsigned_abs()))
                    };
                    self.seek_to(target).await
                }
            };
            if res.is_err() {
                return;
            }
        }
    }

    /// Moves playback to `target`. Seeking backwards rebuilds state from the
    /// start of the files; both directions send the skipped messages as one
    /// burst.
    async fn seek_to(&mut self, target: Duration) -> Result<(), ()> {
        let target = target.min(self.end);
        if target < self.current() || self.reader.is_none() {
            match self.files.open() {
                Ok(r) => self.reader = Some(r),
                Err(e) => {
                    tracing::error!("replay: {e:#}");
                    return Err(());
                }
            }
            self.tx.send(FeedEvent::Reset).await.map_err(drop)?;
        }
        let z_cutoff = target.saturating_sub(Z_KEEP);
        let reader = self.reader.as_mut().ok_or(())?;
        while reader.peek().is_some_and(|t| t <= target) {
            let Some(line) = reader.pop() else { break };
            // Skipped unparsed: most of a long seek is telemetry.
            if line.topic.ends_with(".z") && line.offset < z_cutoff {
                continue;
            }
            let Some(m) = line.parse() else { continue };
            self.tx.send(FeedEvent::Message(m)).await.map_err(drop)?;
        }
        self.set_pos(target);
        self.status().await
    }

    async fn status(&self) -> Result<(), ()> {
        let st = ReplayStatus {
            pos: self.pos,
            at: self.anchor,
            speed: self.speed(),
            paused: self.paused,
            end: self.end,
        };
        self.tx.send(FeedEvent::Replay(st)).await.map_err(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::fmt_offset;

    /// A session with lap `i` at minute `i`, in `dir`.
    fn session(dir: &std::path::Path) -> SessionFiles {
        let text: String = (0..100u64)
            .map(|i| {
                let t = fmt_offset(Duration::from_secs(i * 60));
                format!("{t}{{\"CurrentLap\":{i}}}\n")
            })
            .collect();
        std::fs::write(dir.join("LapCount.jsonStream"), text).unwrap();
        SessionFiles::open_dir(dir, &["LapCount"]).unwrap()
    }

    fn laps(events: &[FeedEvent]) -> Vec<u64> {
        events
            .iter()
            .filter_map(|e| match e {
                FeedEvent::Message(m) => m.data["CurrentLap"].as_u64(),
                _ => None,
            })
            .collect()
    }

    async fn drain(rx: &mut mpsc::Receiver<FeedEvent>) -> Vec<FeedEvent> {
        let mut out = Vec::new();
        while let Ok(Some(e)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            out.push(e);
        }
        out
    }

    #[tokio::test]
    async fn starts_at_offset_and_seeks_back() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel(1000);
        let (ctx, crx) = mpsc::channel(8);
        let replay = Replay::new(session(dir.path()), tx, 1.0);
        tokio::spawn(replay.run(Duration::from_secs(600), crx));

        let first = drain(&mut rx).await;
        assert!(matches!(first[0], FeedEvent::Reset));
        assert_eq!(laps(&first), (0..=10).collect::<Vec<_>>());

        ctx.send(ReplayCmd::Seek(-300)).await.unwrap();
        let back = drain(&mut rx).await;
        assert!(matches!(back[0], FeedEvent::Reset));
        assert_eq!(laps(&back), (0..=5).collect::<Vec<_>>());

        ctx.send(ReplayCmd::Seek(120)).await.unwrap();
        let fwd = drain(&mut rx).await;
        assert!(!fwd.iter().any(|e| matches!(e, FeedEvent::Reset)));
        assert_eq!(laps(&fwd), vec![6, 7]);
    }
}
