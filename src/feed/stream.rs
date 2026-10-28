//! A session's `.jsonStream` files on disk, read as they're played rather
//! than held in memory: a parsed session takes about ten times its text.
//! Each topic's file is in time order, so playing the session is a merge of
//! one read position per file.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::Value;

use super::{FeedMessage, parse_offset};

/// The files of one archived or recorded session.
#[derive(Debug, Clone)]
pub struct SessionFiles {
    /// In topic order: messages at the same offset play in this order.
    files: Vec<(Arc<str>, PathBuf)>,
    /// Offset of the last message.
    pub end: Duration,
}

impl SessionFiles {
    /// The `topics` found in `dir` as `<topic>.jsonStream`.
    pub fn open_dir(dir: &Path, topics: &[&str]) -> anyhow::Result<Self> {
        let mut files = Vec::new();
        for topic in topics {
            let path = dir.join(format!("{topic}.jsonStream"));
            if path.is_file() {
                files.push((Arc::from(*topic), path));
            }
        }
        if files.is_empty() {
            bail!("no .jsonStream files in {}", dir.display());
        }
        let mut s = Self {
            files,
            end: Duration::ZERO,
        };
        // Offsets only: no JSON is parsed.
        let mut r = s.open()?;
        while let Some(line) = r.pop() {
            s.end = s.end.max(line.offset);
        }
        Ok(s)
    }

    /// Reads from the start.
    pub fn open(&self) -> anyhow::Result<Reader> {
        let streams = self
            .files
            .iter()
            .map(|(topic, path)| Stream::open(topic, path))
            .collect::<anyhow::Result<_>>()?;
        Ok(Reader { streams })
    }

    /// Every message of `topic`, in order. For the small topics only.
    pub fn topic(&self, topic: &str) -> Vec<FeedMessage> {
        let Some((t, path)) = self.files.iter().find(|(t, _)| &**t == topic) else {
            return Vec::new();
        };
        let Ok(mut s) = Stream::open(t, path) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while let Some(line) = s.next.take() {
            s.advance();
            out.extend(line.parse());
        }
        out
    }

    /// Offset of the first `SessionStatus: Started`, where replays begin by
    /// default.
    pub fn start(&self) -> Option<Duration> {
        self.topic("SessionStatus")
            .into_iter()
            .find(|m| m.data["Status"] == "Started")
            .map(|m| m.offset)
    }
}

/// The session's messages in time order, read a line at a time.
pub struct Reader {
    streams: Vec<Stream>,
}

impl Reader {
    /// Offset of the next message.
    pub fn peek(&self) -> Option<Duration> {
        self.streams
            .iter()
            .filter_map(|s| s.next.as_ref().map(|l| l.offset))
            .min()
    }

    /// The next message, unparsed. On equal offsets the earlier topic
    /// comes first.
    pub fn pop(&mut self) -> Option<Line> {
        let i = self
            .streams
            .iter()
            .enumerate()
            .filter_map(|(i, s)| Some((s.next.as_ref()?.offset, i)))
            .min()?
            .1;
        let s = &mut self.streams[i];
        let line = s.next.take();
        s.advance();
        line
    }
}

impl Iterator for Reader {
    type Item = FeedMessage;

    /// The next message that parses.
    fn next(&mut self) -> Option<FeedMessage> {
        loop {
            if let Some(m) = self.pop()?.parse() {
                return Some(m);
            }
        }
    }
}

/// One topic file's read position, with its next line read ahead.
struct Stream {
    topic: Arc<str>,
    lines: BufReader<File>,
    buf: String,
    next: Option<Line>,
}

impl Stream {
    fn open(topic: &Arc<str>, path: &Path) -> anyhow::Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut s = Stream {
            topic: topic.clone(),
            lines: BufReader::new(file),
            buf: String::new(),
            next: None,
        };
        s.advance();
        Ok(s)
    }

    /// Reads ahead to the next line with an offset.
    fn advance(&mut self) {
        loop {
            self.buf.clear();
            match self.lines.read_line(&mut self.buf) {
                Ok(0) => return,
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("{}: {e}", self.topic);
                    return;
                }
            }
            if let Some(line) = Line::split(&self.topic, &self.buf) {
                self.next = Some(line);
                return;
            }
        }
    }
}

/// One line of a `.jsonStream` file, its JSON not parsed yet.
#[derive(Debug)]
pub struct Line {
    pub topic: Arc<str>,
    pub offset: Duration,
    json: String,
}

impl Line {
    /// An offset such as `00:12:03.456` immediately followed by JSON.
    fn split(topic: &Arc<str>, line: &str) -> Option<Self> {
        let line = strip_bom(line).trim_end();
        let at = line.find(|c: char| !(c.is_ascii_digit() || c == ':' || c == '.'))?;
        Some(Self {
            topic: topic.clone(),
            offset: parse_offset(&line[..at])?,
            json: line[at..].to_owned(),
        })
    }

    pub fn parse(self) -> Option<FeedMessage> {
        match serde_json::from_str::<Value>(&self.json) {
            Ok(data) => Some(FeedMessage {
                topic: self.topic.to_string(),
                data,
                offset: self.offset,
                utc: None,
            }),
            Err(e) => {
                tracing::warn!("{}: bad line at {:?}: {e}", self.topic, self.offset);
                None
            }
        }
    }
}

fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// Parses `.jsonStream` text held in memory.
#[cfg(test)]
pub fn parse_text(text: &str, topic: &str) -> Vec<FeedMessage> {
    let topic: Arc<str> = Arc::from(topic);
    text.lines()
        .filter_map(|l| Line::split(&topic, l)?.parse())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, topic: &str, lines: &[&str]) {
        std::fs::write(dir.join(format!("{topic}.jsonStream")), lines.join("\r\n")).unwrap();
    }

    #[test]
    fn merges_topics_in_time_order() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "LapCount",
            &[
                "\u{feff}00:00:01.000{\"CurrentLap\":1}",
                "00:00:03.000{\"CurrentLap\":2}",
            ],
        );
        write(
            dir.path(),
            "SessionStatus",
            &[
                "00:00:00.500{\"Status\":\"Inactive\"}",
                "garbage",
                "00:00:03.000{\"Status\":\"Started\"}",
            ],
        );
        let s =
            SessionFiles::open_dir(dir.path(), &["LapCount", "SessionStatus", "Missing"]).unwrap();
        assert_eq!(s.end, Duration::from_secs(3));
        assert_eq!(s.start(), Some(Duration::from_secs(3)));
        let order: Vec<(String, u64)> = s
            .open()
            .unwrap()
            .map(|m| (m.topic, m.offset.as_millis() as u64))
            .collect();
        // Equal offsets: LapCount first, as listed.
        assert_eq!(
            order,
            [
                ("SessionStatus".into(), 500),
                ("LapCount".into(), 1000),
                ("LapCount".into(), 3000),
                ("SessionStatus".into(), 3000),
            ]
        );
    }
}
