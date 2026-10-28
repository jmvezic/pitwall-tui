//! F1's static archive: `Index.json` lookup, and `.jsonStream` download
//! into the on-disk cache that replays read from.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Deserialize;

use super::stream::SessionFiles;

pub const STATIC_URL: &str = "https://livetiming.formula1.com/static/";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Index {
    pub year: i32,
    pub meetings: Vec<Meeting>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Meeting {
    pub name: String,
    #[serde(default)]
    pub location: String,
    pub country: Option<Country>,
    #[serde(default)]
    pub sessions: Vec<Session>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Country {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub code: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Session {
    pub name: String,
    #[serde(default)]
    pub start_date: String,
    pub path: Option<String>,
}

/// Strips the UTF-8 BOM that archive files may start with.
fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

pub fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("pitwall-tui/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()?)
}

/// Fetches the season index, falling back to the cached copy when offline.
pub async fn fetch_index(
    client: &reqwest::Client,
    token: Option<&str>,
    year: i32,
    cache: &Path,
) -> anyhow::Result<Index> {
    let path = cache
        .join("archive")
        .join(year.to_string())
        .join("Index.json");
    let url = format!("{STATIC_URL}{year}/Index.json");
    let text = match get_text(client, token, &url).await {
        Ok(Some(t)) => {
            write_cache(&path, &t);
            t
        }
        Ok(None) => bail!("no archive index for {year}"),
        Err(e) => {
            tracing::warn!("fetching {url}: {e:#}; trying cache");
            std::fs::read_to_string(&path).map_err(|_| e)?
        }
    };
    Ok(serde_json::from_str(strip_bom(&text))?)
}

/// Finds a session by fuzzy meeting and session names, for example
/// `singapore race` or `monza fp2`.
pub fn find_session<'a>(
    index: &'a Index,
    meeting: &str,
    session: &str,
) -> anyhow::Result<(&'a Meeting, &'a Session)> {
    let m = &index.meetings[find_meeting(index, meeting)?];
    Ok((m, find_in_meeting(m, session)?))
}

/// Index of the meeting matching name, location or country, preferring the
/// Grand Prix over pre-season testing at the same venue.
pub fn find_meeting(index: &Index, meeting: &str) -> anyhow::Result<usize> {
    let mq = meeting.to_lowercase();
    let meetings: Vec<(usize, &Meeting)> = index
        .meetings
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            let c = m.country.as_ref();
            [
                m.name.as_str(),
                m.location.as_str(),
                c.map_or("", |c| c.name.as_str()),
                c.map_or("", |c| c.code.as_str()),
            ]
            .iter()
            .any(|f| f.to_lowercase().contains(&mq))
        })
        .collect();
    meetings
        .iter()
        .find(|(_, m)| !m.name.to_lowercase().contains("testing"))
        .or(meetings.first())
        .map(|(i, _)| *i)
        .with_context(|| format!("no meeting matching {meeting:?} in {}", index.year))
}

/// A meeting's session by name or alias (`race`, `quali`, `fp2`, `sq`).
pub fn find_in_meeting<'a>(m: &'a Meeting, session: &str) -> anyhow::Result<&'a Session> {
    let sq = session_alias(&session.to_lowercase());
    m.sessions
        .iter()
        .find(|s| s.name.to_lowercase() == sq)
        .or_else(|| {
            m.sessions
                .iter()
                .find(|s| s.name.to_lowercase().contains(&sq))
        })
        .with_context(|| {
            let names: Vec<_> = m.sessions.iter().map(|s| s.name.as_str()).collect();
            format!(
                "no session {session:?} in {}; have {}",
                m.name,
                names.join(", ")
            )
        })
}

fn session_alias(s: &str) -> String {
    match s {
        "fp1" | "p1" => "practice 1",
        "fp2" | "p2" => "practice 2",
        "fp3" | "p3" => "practice 3",
        "q" | "quali" => "qualifying",
        "sq" | "sprint-quali" | "shootout" => "sprint qualifying",
        "r" | "gp" => "race",
        other => other,
    }
    .to_owned()
}

/// Downloads every topic of an archived session that isn't cached yet, and
/// opens the cached files to be read as they're played.
pub async fn fetch_session(
    client: &reqwest::Client,
    token: Option<&str>,
    session_path: &str,
    topics: &[&str],
    cache: &Path,
) -> anyhow::Result<SessionFiles> {
    let dir = cache.join("archive").join(session_path);
    let fetches = topics.iter().map(|&topic| {
        let dir = dir.clone();
        async move {
            let file = dir.join(format!("{topic}.jsonStream"));
            if file.is_file() {
                return anyhow::Ok(());
            }
            let url = format!("{STATIC_URL}{session_path}{topic}.jsonStream");
            tracing::info!("downloading {url}");
            let Some(t) = get_text(client, token, &url).await? else {
                tracing::warn!("{topic} not in archive");
                return Ok(());
            };
            // Replay reads it from here, so it must be written.
            std::fs::create_dir_all(&dir)
                .and_then(|()| std::fs::write(&file, t))
                .with_context(|| format!("caching {}", file.display()))
        }
    });
    for r in futures_util::future::join_all(fetches).await {
        r?;
    }
    SessionFiles::open_dir(&dir, topics)
        .with_context(|| format!("archive has no data for {session_path}"))
}

/// A GET from the archive, which (as of October 2026) wants the F1 TV token
/// as a bearer header. Only archive requests carry it: the shared client
/// also talks to other hosts.
pub fn get(client: &reqwest::Client, token: Option<&str>, url: &str) -> reqwest::RequestBuilder {
    let req = client.get(url);
    match token {
        Some(t) => req.bearer_auth(t),
        None => req,
    }
}

/// GET returning `None` for a missing file (the archive answers 403 or 404).
async fn get_text(
    client: &reqwest::Client,
    token: Option<&str>,
    url: &str,
) -> anyhow::Result<Option<String>> {
    let resp = get(client, token, url).send().await?;
    match resp.status().as_u16() {
        403 | 404 => return Ok(None),
        401 if token.is_none() => {
            bail!("the F1 archive needs an F1 TV token: run `pitwall login` ({url})")
        }
        401 => {
            bail!("the F1 archive refused the F1 TV token (expired?): run `pitwall login` ({url})")
        }
        _ => {}
    }
    Ok(Some(resp.error_for_status()?.text().await?))
}

fn write_cache(path: &PathBuf, text: &str) {
    let res = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(path, text));
    if let Err(e) = res {
        tracing::warn!("caching {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bom_and_string_payloads() {
        let text = "\u{feff}00:00:04.219{\"Status\":\"1\"}\r\n00:04:33.190\"7ZOx\"\n\ngarbage\n";
        let msgs = crate::feed::stream::parse_text(text, "X");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].offset, Duration::from_millis(4219));
        assert_eq!(msgs[0].data["Status"], "1");
        assert_eq!(msgs[1].data, "7ZOx");
    }

    #[test]
    fn finds_sessions_by_alias() {
        let idx: Index = serde_json::from_str(
            r#"{"Year":2025,"Meetings":[
            {"Name":"Pre-Season Testing","Location":"Sakhir","Country":{"Name":"Bahrain","Code":"BRN"},
             "Sessions":[{"Name":"Day 1","Path":"t/"}]},
            {"Name":"Bahrain Grand Prix","Location":"Sakhir","Country":{"Name":"Bahrain","Code":"BRN"},
             "Sessions":[{"Name":"Practice 1","Path":"a/"},{"Name":"Sprint Qualifying","Path":"b/"},
                         {"Name":"Sprint","Path":"c/"},{"Name":"Race","Path":"d/"}]}]}"#,
        )
        .unwrap();
        let (m, s) = find_session(&idx, "bahrain", "fp1").unwrap();
        assert_eq!(m.name, "Bahrain Grand Prix");
        assert_eq!(s.path.as_deref(), Some("a/"));
        assert_eq!(
            find_session(&idx, "sakhir", "sprint")
                .unwrap()
                .1
                .path
                .as_deref(),
            Some("c/")
        );
        assert_eq!(
            find_session(&idx, "BRN", "sq").unwrap().1.path.as_deref(),
            Some("b/")
        );
        assert!(find_session(&idx, "monza", "race").is_err());
    }
}
