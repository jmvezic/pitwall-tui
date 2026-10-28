//! Writes live messages in the archive's `.jsonStream` format, one file per
//! topic, so recordings replay exactly like archive sessions.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{LineWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde_json::Value;

use super::fmt_offset;

pub struct Recorder {
    dir: PathBuf,
    files: HashMap<String, LineWriter<File>>,
}

impl Recorder {
    pub fn new(dir: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self {
            dir: dir.to_owned(),
            files: HashMap::new(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Appends one message. `topic` keeps its `.z` suffix and `data` its
    /// compressed form, as in the archive.
    pub fn write(&mut self, topic: &str, offset: Duration, data: &Value) -> anyhow::Result<()> {
        if !self.files.contains_key(topic) {
            let path = self.dir.join(format!("{topic}.jsonStream"));
            let f = File::options().create(true).append(true).open(&path)?;
            self.files.insert(topic.to_owned(), LineWriter::new(f));
        }
        let w = self.files.get_mut(topic).expect("inserted above");
        writeln!(w, "{}{}", fmt_offset(offset), data)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::stream::parse_text;

    #[test]
    fn recordings_parse_like_archive_files() {
        let dir = std::env::temp_dir().join(format!("pitwall-rec-{}", std::process::id()));
        let mut r = Recorder::new(&dir).unwrap();
        r.write(
            "LapCount",
            Duration::from_millis(1500),
            &serde_json::json!({"CurrentLap": 4}),
        )
        .unwrap();
        r.write(
            "LapCount",
            Duration::from_millis(61_000),
            &serde_json::json!({"CurrentLap": 5}),
        )
        .unwrap();
        drop(r);
        let text = fs::read_to_string(dir.join("LapCount.jsonStream")).unwrap();
        let msgs = parse_text(&text, "LapCount");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].offset, Duration::from_secs(61));
        assert_eq!(msgs[1].data["CurrentLap"], 5);
        fs::remove_dir_all(dir).ok();
    }
}
