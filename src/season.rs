//! The championship around a session, from Jolpica (the Ergast API's
//! successor): which round it is, the rounds still to come, and each
//! driver's grand prix wins so far, for the title tie-break. Points come
//! from F1's own feed (`ChampionshipPrediction`), which already counts a
//! sprint run earlier the same weekend.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, bail};
use chrono::{Datelike, NaiveDate};
use serde_json::Value;

const API: &str = "https://api.jolpi.ca/ergast/f1";

#[derive(Debug, Clone, Default)]
pub struct Season {
    pub year: i32,
    pub round: u32,
    /// Grand prix wins before this round, by driver code (`ANT`).
    pub wins: HashMap<String, u32>,
    /// The rounds after this one: whether each has a sprint.
    pub later: Vec<bool>,
}

/// Loads the season around `date`, a session's day: the first round whose
/// grand prix is on or after it. Falls back to the cache when offline.
pub async fn load(
    client: &reqwest::Client,
    cache: &Path,
    date: NaiveDate,
) -> anyhow::Result<Season> {
    let year = date.year();
    let races = get(
        client,
        cache,
        &format!("{year}/races.json?limit=100"),
        &format!("{year}_races"),
    )
    .await?;
    let races = races["MRData"]["RaceTable"]["Races"]
        .as_array()
        .context("no races in the schedule")?;
    let day = |r: &Value| NaiveDate::parse_from_str(r["date"].as_str()?, "%Y-%m-%d").ok();
    let at = races
        .iter()
        .position(|r| day(r).is_some_and(|d| d >= date))
        .with_context(|| format!("no grand prix on or after {date}"))?;
    let round: u32 = races[at]["round"]
        .as_str()
        .and_then(|r| r.parse().ok())
        .context("round without a number")?;
    let later = races[at + 1..]
        .iter()
        .map(|r| r.get("Sprint").is_some())
        .collect();

    let mut wins = HashMap::new();
    if round > 1 {
        let before = round - 1;
        let path = format!("{year}/{before}/driverStandings.json?limit=100");
        let st = get(client, cache, &path, &format!("{year}_{before}_standings")).await?;
        let Some(list) =
            st["MRData"]["StandingsTable"]["StandingsLists"][0]["DriverStandings"].as_array()
        else {
            bail!("Jolpica has no standings after round {before} yet");
        };
        for d in list {
            let code = d["Driver"]["code"].as_str();
            let n = d["wins"].as_str().and_then(|w| w.parse().ok());
            if let (Some(code), Some(n)) = (code, n) {
                wins.insert(code.to_owned(), n);
            }
        }
    }
    Ok(Season {
        year,
        round,
        wins,
        later,
    })
}

/// GETs `path` from Jolpica, caching it as `name`; the cached copy when
/// the request fails.
async fn get(
    client: &reqwest::Client,
    cache: &Path,
    path: &str,
    name: &str,
) -> anyhow::Result<Value> {
    let file = cache.join("jolpica").join(format!("{name}.json"));
    let url = format!("{API}/{path}");
    let fetched = async {
        let text = client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        serde_json::from_str::<Value>(&text)?;
        anyhow::Ok(text)
    }
    .await;
    let text = match fetched {
        Ok(t) => {
            let res = std::fs::create_dir_all(file.parent().unwrap_or(cache))
                .and_then(|()| std::fs::write(&file, &t));
            if let Err(e) = res {
                tracing::warn!("caching {}: {e}", file.display());
            }
            t
        }
        Err(e) => {
            tracing::warn!("fetching {url}: {e:#}; trying cache");
            std::fs::read_to_string(&file).map_err(|_| e)?
        }
    };
    Ok(serde_json::from_str(&text)?)
}
