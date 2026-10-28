//! `pitwall login`: signs in to F1 TV in a browser window pitwall opens
//! (see [`crate::signin`]) or, with `--browser`, finds the `login-session`
//! cookie in a browser's cookie store, and saves its subscription token to
//! the config.
//!
//! Only cookies named `login-session` on `formula1.com` hosts are read.
//! The token is never printed.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};

use crate::{config, signin};

const COOKIE: &str = "login-session";
const HOST_SUFFIX: &str = "formula1.com";

/// Browsers built on Chromium share one cookie format; they differ in
/// where they keep their profiles and what their Keychain entry is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chromium {
    pub name: &'static str,
    /// Relative to `~/Library/Application Support`.
    dir: &'static str,
    keychain: &'static str,
}

pub const CHROMIUMS: &[Chromium] = &[
    Chromium {
        name: "Chrome",
        dir: "Google/Chrome",
        keychain: "Chrome Safe Storage",
    },
    Chromium {
        name: "Brave",
        dir: "BraveSoftware/Brave-Browser",
        keychain: "Brave Safe Storage",
    },
    Chromium {
        name: "Edge",
        dir: "Microsoft Edge",
        keychain: "Microsoft Edge Safe Storage",
    },
    Chromium {
        name: "Arc",
        dir: "Arc/User Data",
        keychain: "Arc Safe Storage",
    },
    Chromium {
        name: "Vivaldi",
        dir: "Vivaldi",
        keychain: "Vivaldi Safe Storage",
    },
    Chromium {
        name: "Opera",
        dir: "com.operasoftware.Opera",
        keychain: "Opera Safe Storage",
    },
    Chromium {
        name: "Chromium",
        dir: "Chromium",
        keychain: "Chromium Safe Storage",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Browser {
    Chromium(Chromium),
    Firefox,
    Safari,
}

impl Browser {
    pub fn all() -> Vec<Browser> {
        let mut v: Vec<Browser> = CHROMIUMS.iter().map(|c| Browser::Chromium(*c)).collect();
        v.push(Browser::Firefox);
        v.push(Browser::Safari);
        v
    }

    pub fn name(&self) -> &'static str {
        match self {
            Browser::Chromium(c) => c.name,
            Browser::Firefox => "Firefox",
            Browser::Safari => "Safari",
        }
    }

    pub fn by_name(name: &str) -> Option<Browser> {
        Browser::all()
            .into_iter()
            .find(|b| b.name().eq_ignore_ascii_case(name))
    }

    /// Cookie values found in this browser, labelled by profile.
    fn find(&self, home: &Path) -> anyhow::Result<Vec<Found>> {
        let support = home.join("Library/Application Support");
        match self {
            Browser::Chromium(c) => chromium_find(c, &support.join(c.dir), keychain_password),
            Browser::Firefox => firefox_find(&support.join("Firefox/Profiles")),
            Browser::Safari => safari_find(home),
        }
    }
}

#[derive(Debug)]
struct Found {
    profile: String,
    value: String,
}

/// A token found in a browser.
struct Candidate {
    browser: &'static str,
    profile: String,
    token: String,
    expires: DateTime<Utc>,
}

/// Prints where each browser's cookies were looked for and the names (never
/// the values) of the formula1.com cookies there.
fn diagnose(b: &Browser, home: &Path) {
    let support = home.join("Library/Application Support");
    let names_query = |table: &str, host_col: &str| {
        format!(
            "SELECT {host_col}, name FROM {table} WHERE {host_col} LIKE '%{HOST_SUFFIX}' \
             ORDER BY {host_col}, name"
        )
    };
    let report = |label: &str, db: &Path, table: &str, host_col: &str| {
        let res = with_db_copy(db, |conn| {
            let total: i64 =
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
            let mut stmt = conn.prepare(&names_query(table, host_col))?;
            let names = stmt
                .query_map([], |r| {
                    Ok(format!(
                        "{} {}",
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((total, names))
        });
        match res {
            Ok((total, names)) => {
                eprintln!(
                    "  {label}: {total} cookies, {} on {HOST_SUFFIX}",
                    names.len()
                );
                for n in names {
                    eprintln!("    {n}");
                }
            }
            Err(e) => eprintln!("  {label}: {e:#}"),
        }
    };
    match b {
        Browser::Chromium(c) => {
            let root = support.join(c.dir);
            eprintln!("{} — {} ({})", c.name, root.display(), exists(&root));
            match chromium_profiles(&root) {
                Ok(profiles) => {
                    for (profile, db) in profiles {
                        report(
                            &format!("{profile} [{}]", db.display()),
                            &db,
                            "cookies",
                            "host_key",
                        );
                    }
                }
                Err(e) if root.exists() => eprintln!("  {e:#}"),
                Err(_) => {}
            }
        }
        Browser::Firefox => {
            let root = support.join("Firefox/Profiles");
            eprintln!("Firefox — {} ({})", root.display(), exists(&root));
            for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
                let db = e.path().join("cookies.sqlite");
                if db.is_file() {
                    report(&e.file_name().to_string_lossy(), &db, "moz_cookies", "host");
                }
            }
        }
        Browser::Safari => match safari_find(home) {
            Ok(f) => eprintln!(
                "Safari — {} login-session cookie(s) on {HOST_SUFFIX}",
                f.len()
            ),
            Err(e) => eprintln!("Safari — {e:#}"),
        },
    }
}

fn exists(p: &Path) -> &'static str {
    if p.is_dir() { "found" } else { "not found" }
}

/// Without `browser`, signs in through a window on `profile`; with it,
/// searches that browser's cookies (`all`: every supported browser). Saves
/// the token to `config_file` and reports what it did.
pub fn run(
    browser: Option<&str>,
    config_file: &Path,
    profile: &Path,
    debug: bool,
) -> anyhow::Result<()> {
    let Some(browser) = browser else {
        if !signin::available() {
            eprintln!(
                "No Chrome, Brave, Edge or Chromium found to sign in with; \
                 looking for an F1 TV sign-in in your browsers instead."
            );
            return from_browsers(None, config_file, debug);
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        eprintln!("Sign in to F1 TV in the browser window that opens (close it to cancel).");
        let t = rt.block_on(signin::sign_in(profile))?;
        config::save_token(config_file, &t.token)?;
        println!(
            "Signed in. F1 TV token valid until {}.",
            t.expires.format("%Y-%m-%d %H:%M UTC")
        );
        println!("Saved to {}", config_file.display());
        return Ok(());
    };
    let browser = Some(browser).filter(|b| !b.eq_ignore_ascii_case("all"));
    from_browsers(browser, config_file, debug)
}

/// Searches `browser` (or every supported browser), saves the token that
/// expires last to `config_file`, and reports what it did.
fn from_browsers(browser: Option<&str>, config_file: &Path, debug: bool) -> anyhow::Result<()> {
    let browsers = match browser {
        Some(name) => vec![Browser::by_name(name).with_context(|| {
            let names: Vec<_> = Browser::all().iter().map(|b| b.name()).collect();
            format!("unknown browser {name:?}; supported: {}", names.join(", "))
        })?],
        None => Browser::all(),
    };
    let home = directories::BaseDirs::new()
        .context("no home directory")?
        .home_dir()
        .to_owned();

    let mut candidates = Vec::new();
    for b in &browsers {
        if debug {
            diagnose(b, &home);
        }
        match b.find(&home) {
            Ok(found) => {
                for f in found {
                    match config::normalise_token(&f.value)
                        .and_then(|t| Some((config::token_expiry(&t)?, t)))
                    {
                        Some((expires, token)) => candidates.push(Candidate {
                            browser: b.name(),
                            profile: f.profile,
                            token,
                            expires,
                        }),
                        None => eprintln!(
                            "{} ({}): found an F1 login cookie but no subscription token in it",
                            b.name(),
                            f.profile
                        ),
                    }
                }
            }
            Err(e) => eprintln!("{}: {e:#}", b.name()),
        }
    }

    let Some(best) = candidates.into_iter().max_by_key(|c| c.expires) else {
        bail!(
            "no F1 TV login found{}. Sign in at https://f1tv.formula1.com and run `pitwall login --browser` again.",
            browser.map(|b| format!(" in {b}")).unwrap_or_default()
        );
    };
    if best.expires <= Utc::now() {
        bail!(
            "the F1 TV login in {} ({}) expired on {}. Sign in again at https://f1tv.formula1.com.",
            best.browser,
            best.profile,
            best.expires.format("%Y-%m-%d")
        );
    }
    config::save_token(config_file, &best.token)?;
    println!(
        "Found an F1 TV token in {} ({}), valid until {}.",
        best.browser,
        best.profile,
        best.expires.format("%Y-%m-%d %H:%M UTC")
    );
    println!("Saved to {}", config_file.display());
    Ok(())
}

// --- Chromium ---------------------------------------------------------------

/// Profile directories that have a cookie database. A browser that isn't
/// installed has none; a folder macOS won't let us read is an error, not
/// an empty result.
fn chromium_profiles(root: &Path) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(access_error(root, e)),
    };
    let mut v = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| access_error(root, e))?;
        // The browser folder also holds plain files (e.g. `Local State`).
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        for db in [
            entry.path().join("Cookies"),
            entry.path().join("Network/Cookies"),
        ] {
            match std::fs::metadata(&db) {
                Ok(m) if m.is_file() => {
                    v.push((entry.file_name().to_string_lossy().into_owned(), db));
                    break;
                }
                // Only a permission error means we're being kept out.
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    return Err(access_error(&db, e));
                }
                _ => {}
            }
        }
    }
    if v.is_empty() {
        bail!(
            "{} exists but no profile in it has a cookie database. If you are signed in, \
             macOS may be hiding it: {FDA_HINT}",
            root.display()
        );
    }
    v.sort();
    Ok(v)
}

const FDA_HINT: &str = "give your terminal app Full Disk Access (System Settings → \
     Privacy & Security → Full Disk Access), restart the terminal, and try again";

/// Explains a failed read; macOS reports its privacy protection of other
/// apps' data as a permission error.
fn access_error(path: &Path, e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        anyhow::anyhow!("macOS blocked reading {} ({e}); {FDA_HINT}", path.display())
    } else {
        anyhow::anyhow!("reading {}: {e}", path.display())
    }
}

/// `password` fetches the Keychain secret; it is only called when a
/// matching cookie exists, so browsers without one never prompt.
fn chromium_find(
    c: &Chromium,
    root: &Path,
    password: impl Fn(&str) -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<Vec<Found>> {
    let mut found = Vec::new();
    let mut key: Option<[u8; 16]> = None;
    if !root.exists() {
        return Ok(Vec::new());
    }
    for (profile, db) in chromium_profiles(root)? {
        let rows: Vec<(String, Vec<u8>, String)> = with_db_copy(&db, |conn| {
            let mut stmt = conn.prepare(
                "SELECT host_key, encrypted_value, value FROM cookies \
                 WHERE name = ?1 AND host_key LIKE ?2",
            )?;
            let rows = stmt
                .query_map((COOKIE, format!("%{HOST_SUFFIX}")), |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?
                .collect::<Result<_, _>>()?;
            Ok(rows)
        })
        .with_context(|| format!("reading {}", db.display()))?;
        for (host, encrypted, plain) in rows {
            let value = if encrypted.is_empty() {
                plain
            } else {
                let k = match key {
                    Some(k) => k,
                    None => {
                        let pw = password(c.keychain)?;
                        let k = chromium_key(&pw);
                        key = Some(k);
                        k
                    }
                };
                chromium_decrypt(&k, &host, &encrypted)?
            };
            found.push(Found {
                profile: profile.clone(),
                value,
            });
        }
    }
    Ok(found)
}

/// Reads the browser's cookie-encryption secret from the macOS Keychain.
/// macOS asks the user to allow this.
fn keychain_password(service: &str) -> anyhow::Result<Vec<u8>> {
    if !cfg!(target_os = "macos") {
        bail!("reading Chromium cookies is only supported on macOS");
    }
    let out = Command::new("security")
        .args(["find-generic-password", "-w", "-s", service])
        .output()
        .context("running `security`")?;
    if !out.status.success() {
        bail!(
            "couldn't read \"{service}\" from the Keychain (access denied, or the entry is missing)"
        );
    }
    let mut pw = out.stdout;
    while pw.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        pw.pop();
    }
    Ok(pw)
}

/// Chromium on macOS: AES-128 key = PBKDF2-HMAC-SHA1(password, "saltysalt", 1003).
fn chromium_key(password: &[u8]) -> [u8; 16] {
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, b"saltysalt", 1003, &mut key);
    key
}

/// Decrypts a `v10` cookie: AES-128-CBC, IV of 16 spaces, PKCS#7 padding.
/// Since Chromium 130 the plaintext starts with SHA-256(host), which is
/// stripped.
fn chromium_decrypt(key: &[u8; 16], host: &str, data: &[u8]) -> anyhow::Result<String> {
    use cbc::cipher::block_padding::Pkcs7;
    use cbc::cipher::{BlockModeDecrypt, KeyIvInit};
    use sha2::Digest;

    let Some(body) = data.strip_prefix(b"v10") else {
        bail!("unsupported cookie encryption (not v10)");
    };
    let iv = [b' '; 16];
    let mut plain = cbc::Decryptor::<aes::Aes128>::new(key.into(), &iv.into())
        .decrypt_padded_vec::<Pkcs7>(body)
        .map_err(|_| anyhow::anyhow!("cookie didn't decrypt; wrong Keychain secret?"))?;
    let host_hash = sha2::Sha256::digest(host.as_bytes());
    if plain.len() >= 32 && plain[..32] == host_hash[..] {
        plain.drain(..32);
    }
    String::from_utf8(plain).context("decrypted cookie isn't text")
}

// --- Firefox ----------------------------------------------------------------

fn firefox_find(profiles: &Path) -> anyhow::Result<Vec<Found>> {
    let Ok(entries) = std::fs::read_dir(profiles) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for e in entries.flatten() {
        let db = e.path().join("cookies.sqlite");
        if !db.is_file() {
            continue;
        }
        let values: Vec<String> = with_db_copy(&db, |conn| {
            let mut stmt =
                conn.prepare("SELECT value FROM moz_cookies WHERE name = ?1 AND host LIKE ?2")?;
            let rows = stmt
                .query_map((COOKIE, format!("%{HOST_SUFFIX}")), |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            Ok(rows)
        })
        .with_context(|| format!("reading {}", db.display()))?;
        let profile = e.file_name().to_string_lossy().into_owned();
        found.extend(values.into_iter().map(|value| Found {
            profile: profile.clone(),
            value,
        }));
    }
    Ok(found)
}

// --- Safari -----------------------------------------------------------------

fn safari_find(home: &Path) -> anyhow::Result<Vec<Found>> {
    let paths = [
        home.join("Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies"),
        home.join("Library/Cookies/Cookies.binarycookies"),
    ];
    for path in paths {
        match std::fs::read(&path) {
            Ok(bytes) => {
                return Ok(parse_binarycookies(&bytes)?
                    .into_iter()
                    .filter(|c| c.name == COOKIE && c.domain.ends_with(HOST_SUFFIX))
                    .map(|c| Found {
                        profile: "default".into(),
                        value: c.value,
                    })
                    .collect());
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => bail!(
                "no access to Safari's cookies; give your terminal Full Disk Access \
                 (System Settings → Privacy & Security) and try again"
            ),
            Err(_) => continue,
        }
    }
    Ok(Vec::new())
}

#[derive(Debug, PartialEq)]
struct SafariCookie {
    domain: String,
    name: String,
    value: String,
}

/// Parses Safari's `Cookies.binarycookies`: a big-endian page table, then
/// pages of little-endian cookie records whose strings are referenced by
/// offsets from the record start.
fn parse_binarycookies(b: &[u8]) -> anyhow::Result<Vec<SafariCookie>> {
    let be = |at: usize| -> anyhow::Result<u32> {
        Ok(u32::from_be_bytes(
            b.get(at..at + 4).context("truncated")?.try_into()?,
        ))
    };
    if b.get(..4) != Some(b"cook") {
        bail!("not a binarycookies file");
    }
    let pages = be(4)? as usize;
    let mut sizes = Vec::with_capacity(pages);
    for i in 0..pages {
        sizes.push(be(8 + i * 4)? as usize);
    }
    let mut cookies = Vec::new();
    let mut page_start = 8 + pages * 4;
    for size in sizes {
        let page = b
            .get(page_start..page_start + size)
            .context("truncated page")?;
        page_start += size;
        let le = |at: usize| -> anyhow::Result<usize> {
            Ok(u32::from_le_bytes(page.get(at..at + 4).context("truncated")?.try_into()?) as usize)
        };
        let count = le(4)?;
        for i in 0..count {
            let at = le(8 + i * 4)?;
            let rec = page.get(at..).context("bad cookie offset")?;
            let off = |field: usize| -> anyhow::Result<usize> {
                Ok(
                    u32::from_le_bytes(rec.get(field..field + 4).context("truncated")?.try_into()?)
                        as usize,
                )
            };
            let cstr = |o: usize| -> anyhow::Result<String> {
                let s = rec.get(o..).context("bad string offset")?;
                let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
                Ok(String::from_utf8_lossy(&s[..end]).into_owned())
            };
            cookies.push(SafariCookie {
                domain: cstr(off(16)?)?,
                name: cstr(off(20)?)?,
                value: cstr(off(28)?)?,
            });
        }
    }
    Ok(cookies)
}

// --- shared -----------------------------------------------------------------

/// Opens a copy of a cookie database: browsers keep theirs locked while
/// running. The write-ahead log is copied too, so recent cookies are seen.
fn with_db_copy<T>(
    db: &Path,
    f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
) -> anyhow::Result<T> {
    let dir = tempfile::tempdir()?;
    let copy = dir.path().join("cookies.db");
    std::fs::copy(db, &copy).map_err(|e| access_error(db, e))?;
    for ext in ["-wal", "-journal"] {
        let side = PathBuf::from(format!("{}{ext}", db.display()));
        if side.is_file() {
            let _ = std::fs::copy(&side, dir.path().join(format!("cookies.db{ext}")));
        }
    }
    let conn = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    Ok(f(&conn)?)
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

    /// The cookie value F1 TV sets: URL-encoded JSON holding the token.
    fn cookie_value(token: &str) -> String {
        format!("%7B%22data%22%3A%7B%22subscriptionToken%22%3A%22{token}%22%7D%7D")
    }

    fn encrypt(key: &[u8; 16], host: &str, plain: &str, with_hash: bool) -> Vec<u8> {
        use cbc::cipher::block_padding::Pkcs7;
        use cbc::cipher::{BlockModeEncrypt, KeyIvInit};
        use sha2::Digest;
        let mut msg = Vec::new();
        if with_hash {
            msg.extend_from_slice(&sha2::Sha256::digest(host.as_bytes()));
        }
        msg.extend_from_slice(plain.as_bytes());
        let mut out = b"v10".to_vec();
        out.extend(
            cbc::Encryptor::<aes::Aes128>::new(key.into(), &[b' '; 16].into())
                .encrypt_padded_vec::<Pkcs7>(&msg),
        );
        out
    }

    #[test]
    fn decrypts_old_and_new_chromium_cookies() {
        let key = chromium_key(b"peanuts");
        for with_hash in [false, true] {
            let enc = encrypt(&key, ".formula1.com", "hello", with_hash);
            assert_eq!(
                chromium_decrypt(&key, ".formula1.com", &enc).unwrap(),
                "hello"
            );
        }
        let wrong = chromium_key(b"other");
        let enc = encrypt(&key, ".formula1.com", "hello", false);
        assert!(chromium_decrypt(&wrong, ".formula1.com", &enc).is_err());
        assert!(chromium_decrypt(&key, "x", b"v11abc").is_err());
    }

    #[test]
    fn chromium_profiles_only_prompt_when_needed() {
        let root = tempfile::tempdir().unwrap();
        let key = chromium_key(b"secret");
        let token = jwt(2_000_000_000);
        for (profile, rows) in [
            (
                "Default",
                vec![(".formula1.com", COOKIE, cookie_value(&token))],
            ),
            (
                "Profile 1",
                vec![(".example.com", COOKIE, "nope".to_owned())],
            ),
        ] {
            let dir = root.path().join(profile);
            std::fs::create_dir_all(&dir).unwrap();
            let conn = Connection::open(dir.join("Cookies")).unwrap();
            conn.execute_batch(
                "CREATE TABLE cookies (host_key TEXT, name TEXT, value TEXT, encrypted_value BLOB);",
            )
            .unwrap();
            for (host, name, value) in rows {
                conn.execute(
                    "INSERT INTO cookies VALUES (?1, ?2, '', ?3)",
                    (host, name, encrypt(&key, host, &value, true)),
                )
                .unwrap();
            }
        }
        // Real Chrome folders hold plain files next to the profiles.
        std::fs::write(root.path().join("VariationsSeedV2"), "x").unwrap();
        std::fs::write(root.path().join("Local State"), "{}").unwrap();
        let chrome = CHROMIUMS[0];
        let asked = std::cell::Cell::new(0);
        let found = chromium_find(&chrome, root.path(), |service| {
            assert_eq!(service, "Chrome Safe Storage");
            asked.set(asked.get() + 1);
            Ok(b"secret".to_vec())
        })
        .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].profile, "Default");
        assert_eq!(config::normalise_token(&found[0].value).unwrap(), token);
        assert_eq!(asked.get(), 1);

        // No F1 cookie anywhere: the Keychain is never touched.
        std::fs::remove_dir_all(root.path().join("Default")).unwrap();
        let none = chromium_find(&chrome, root.path(), |_| panic!("prompted")).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn unreadable_profiles_say_why() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let locked = root.path().join("Chrome");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let err = chromium_profiles(&locked).unwrap_err().to_string();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err.contains("Full Disk Access"), "{err}");

        // Readable but no cookie database anywhere.
        std::fs::create_dir(locked.join("Default")).unwrap();
        let err = chromium_profiles(&locked).unwrap_err().to_string();
        assert!(err.contains("no profile"), "{err}");

        // Not installed: nothing to report, not an error.
        let chrome = CHROMIUMS[0];
        let missing = root.path().join("nope");
        assert!(
            chromium_find(&chrome, &missing, |_| panic!())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn reads_firefox_profiles() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("abc.default-release");
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("cookies.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE moz_cookies (host TEXT, name TEXT, value TEXT);
             INSERT INTO moz_cookies VALUES ('.formula1.com', 'login-session', 'v1');
             INSERT INTO moz_cookies VALUES ('.formula1.com', 'other', 'v2');
             INSERT INTO moz_cookies VALUES ('evil.com', 'login-session', 'v3');",
        )
        .unwrap();
        drop(conn);
        let found = firefox_find(root.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].value, "v1");
        assert_eq!(found[0].profile, "abc.default-release");
    }

    /// Builds a one-page binarycookies file.
    fn binarycookies(cookies: &[(&str, &str, &str)]) -> Vec<u8> {
        let mut records = Vec::new();
        for (domain, name, value) in cookies {
            let header = 56u32;
            let mut strings = Vec::new();
            let mut offs = Vec::new();
            for s in [*domain, *name, "/", *value] {
                offs.push(header + strings.len() as u32);
                strings.extend_from_slice(s.as_bytes());
                strings.push(0);
            }
            let mut rec = Vec::new();
            let size = header + strings.len() as u32;
            for v in [size, 1, 0, 0, offs[0], offs[1], offs[2], offs[3]] {
                rec.extend_from_slice(&v.to_le_bytes());
            }
            rec.extend_from_slice(&[0u8; 8]); // comment / end marker
            rec.extend_from_slice(&0f64.to_le_bytes()); // expiry
            rec.extend_from_slice(&0f64.to_le_bytes()); // creation
            rec.extend_from_slice(&strings);
            records.push(rec);
        }
        let mut page = vec![0, 0, 1, 0];
        page.extend_from_slice(&(records.len() as u32).to_le_bytes());
        let table = 8 + records.len() * 4 + 4;
        let mut at = table;
        for r in &records {
            page.extend_from_slice(&(at as u32).to_le_bytes());
            at += r.len();
        }
        page.extend_from_slice(&[0; 4]);
        for r in &records {
            page.extend_from_slice(r);
        }
        let mut file = b"cook".to_vec();
        file.extend_from_slice(&1u32.to_be_bytes());
        file.extend_from_slice(&(page.len() as u32).to_be_bytes());
        file.extend_from_slice(&page);
        file
    }

    #[test]
    fn parses_safari_cookies() {
        let file = binarycookies(&[
            (".formula1.com", "login-session", "tok"),
            ("apple.com", "x", "y"),
        ]);
        let cookies = parse_binarycookies(&file).unwrap();
        assert_eq!(
            cookies[0],
            SafariCookie {
                domain: ".formula1.com".into(),
                name: "login-session".into(),
                value: "tok".into()
            }
        );
        assert_eq!(cookies[1].name, "x");
        assert!(parse_binarycookies(b"nope").is_err());
        assert!(parse_binarycookies(&file[..20]).is_err());
    }

    #[test]
    fn browsers_by_name() {
        assert_eq!(Browser::by_name("chrome").unwrap().name(), "Chrome");
        assert_eq!(Browser::by_name("FIREFOX"), Some(Browser::Firefox));
        assert!(Browser::by_name("netscape").is_none());
    }
}
