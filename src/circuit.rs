//! Circuit outlines and marshal sector positions from MultiViewer's
//! (unofficial) API, cached on disk per circuit and year.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use serde::{Deserialize, Serialize};

const API: &str = "https://api.multiviewer.app/api/v1/circuits";
/// How long a recorded outline stands in before MultiViewer is asked again.
const RETRY: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Circuit {
    pub x: Vec<f64>,
    pub y: Vec<f64>,
    #[serde(default)]
    pub rotation: f64,
    #[serde(default)]
    pub marshal_sectors: Vec<Marker>,
    #[serde(default)]
    pub corners: Vec<Marker>,
    /// Outline index at the end of each timing mini-sector.
    #[serde(default)]
    pub mini_sectors_indexes: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Marker {
    pub number: u32,
    pub track_position: Point,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Circuit {
    /// An outline recorded from one car's positions over a lap.
    pub fn from_samples(pts: &[(f64, f64)]) -> Self {
        Self {
            x: pts.iter().map(|p| p.0).collect(),
            y: pts.iter().map(|p| p.1).collect(),
            rotation: 0.0,
            marshal_sectors: Vec::new(),
            corners: Vec::new(),
            mini_sectors_indexes: Vec::new(),
        }
    }
}

fn cache_file(cache: &Path, key: i64, year: i32) -> PathBuf {
    cache.join("circuits").join(format!("{key}_{year}_v2.json"))
}

/// Where an outline recorded from car positions is kept, apart from
/// MultiViewer's so that it never stands in for one for good.
fn fallback_file(cache: &Path, key: i64, year: i32) -> PathBuf {
    cache
        .join("circuits")
        .join(format!("{key}_{year}_samples.json"))
}

/// Reads the cached circuit or fetches it. Where MultiViewer has none, a
/// recorded outline ([`save_fallback`]) stands in, and MultiViewer is asked
/// again once a day.
pub async fn load(
    client: &reqwest::Client,
    cache: &Path,
    key: i64,
    year: i32,
) -> anyhow::Result<Circuit> {
    if let Some(c) = read(&cache_file(cache, key, year)).await {
        return Ok(c);
    }
    let fallback_path = fallback_file(cache, key, year);
    let fallback = read(&fallback_path).await;
    if let Some(c) = &fallback
        && !retry_due(&fallback_path)
    {
        return Ok(c.clone());
    }
    match fetch(client, key, year).await {
        Ok(c) => {
            write(&cache_file(cache, key, year), &c);
            let _ = tokio::fs::remove_file(&fallback_path).await;
            Ok(c)
        }
        Err(e) => {
            let Some(c) = fallback else {
                return Err(e);
            };
            tracing::info!("still no circuit from MultiViewer: {e:#}");
            touch(&fallback_path);
            Ok(c)
        }
    }
}

async fn fetch(client: &reqwest::Client, key: i64, year: i32) -> anyhow::Result<Circuit> {
    let url = format!("{API}/{key}/{year}");
    tracing::info!("fetching circuit {url}");
    let text = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let c: Circuit = serde_json::from_str(&text).with_context(|| format!("parsing {url}"))?;
    if c.x.len() < 10 || c.x.len() != c.y.len() {
        anyhow::bail!("{url}: outline has {} points", c.x.len());
    }
    Ok(c)
}

async fn read(file: &Path) -> Option<Circuit> {
    let text = tokio::fs::read_to_string(file).await.ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether MultiViewer was last asked a day or more ago: when the fallback
/// was saved or last touched.
fn retry_due(fallback: &Path) -> bool {
    std::fs::metadata(fallback)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_none_or(|age| age >= RETRY)
}

/// Marks MultiViewer as just asked.
fn touch(file: &Path) {
    let res = std::fs::File::options()
        .write(true)
        .open(file)
        .and_then(|f| f.set_modified(SystemTime::now()));
    if let Err(e) = res {
        tracing::warn!("touching {}: {e}", file.display());
    }
}

/// Caches an outline recorded from car positions, for when MultiViewer has
/// no circuit.
pub fn save_fallback(cache: &Path, key: i64, year: i32, c: &Circuit) {
    write(&fallback_file(cache, key, year), c);
}

fn write(file: &Path, c: &Circuit) {
    let res = file
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(file, serde_json::to_vec(c).unwrap_or_default()));
    if let Err(e) = res {
        tracing::warn!("caching circuit to {}: {e}", file.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiviewer_shape() {
        let c: Circuit = serde_json::from_str(
            r#"{"x":[1,2,3],"y":[4,5,6],"rotation":44,"pitLoss":{"normal":"20.81"},
                "marshalSectors":[{"angle":-128.9,"length":0,"number":1,"trackPosition":{"x":-1185,"y":-1341}}],
                "corners":[],"miniSectorsIndexes":[19,28]}"#,
        )
        .unwrap();
        assert_eq!(c.rotation, 44.0);
        assert_eq!(c.marshal_sectors[0].number, 1);
        assert_eq!(c.marshal_sectors[0].track_position.x, -1185.0);
        assert_eq!(c.mini_sectors_indexes, vec![19, 28]);
    }

    #[test]
    fn retries_multiviewer_once_a_day() {
        let dir = tempfile::tempdir().unwrap();
        let c = Circuit::from_samples(&[(0.0, 0.0); 10]);
        save_fallback(dir.path(), 12, 2026, &c);
        let file = fallback_file(dir.path(), 12, 2026);
        assert!(!retry_due(&file));

        let day_ago = SystemTime::now() - RETRY;
        let f = std::fs::File::options().write(true).open(&file).unwrap();
        f.set_modified(day_ago).unwrap();
        assert!(retry_due(&file));

        touch(&file);
        assert!(!retry_due(&file));
        // Never under MultiViewer's name.
        assert!(!cache_file(dir.path(), 12, 2026).exists());
        assert!(retry_due(&dir.path().join("missing.json")));
    }
}
