//! WordPress sync: the bundle, pushed to a WordPress site.
//!
//! The mechanism, settled by the 2026-11-15 recon from WordPress's
//! own developer docs (the simplest door): markdown-wizard pushes to
//! a self-hosted site's core REST API with an Application Password —
//! WordPress >= 5.6, HTTPS, no plugin, no registration. On publish,
//! each publishable file's already-rendered HTML is POSTed to
//! `/wp-json/wp/v2/posts`; the returned post ID is kept in a
//! per-bundle mapping (file path -> post ID). On re-publish the same
//! posts are updated in place — same permalink, no duplicates. A file
//! that leaves the publish set is NOT deleted on WordPress; its
//! mapping is simply dropped.
//!
//! The bundle stays the source of truth, in both directions of
//! failure: the push runs only after the local commit has landed,
//! and a WordPress failure never fails or rolls back a publish. The
//! per-file outcome (created / updated / failed, with the site's own
//! error) is recorded with the bundle and handed back to the editor
//! with the publish result; a retry re-pushes without re-publishing.
//!
//! The application password is a credential. It is stored server-side
//! only, in `<slug>/wordpress.txt` with owner-only permissions; it is
//! never returned by any API (reads report only `password_set`),
//! never logged, and never enters the bundle, the download, or a
//! feed. The mapping and the last sync report live beside it in
//! `<slug>/wp-sync.txt` — both are the bundle's own bookkeeping, not
//! part of any published version, so they are not copied into `prev/`
//! when a version is preserved.
//!
//! WordPress.com-hosted sites are out of scope: their door is an
//! OAuth2 flow, not an application password. A wordpress.com address
//! is refused at save time with that said plainly, rather than
//! failing mysteriously at push time.
//!
//! The client is `ureq` — the one HTTP client in the tree (the
//! server side stays std-only), the same client phext-notepad's
//! edit-sync runs. Sending needs TLS; this is where it lives.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::bundle::{incipit, markdown_stem, ordered_files};
use crate::http::json_escape;
use crate::render::render_markdown;
use crate::store::Bundle;

/// The per-bundle WordPress connection. `password` is the site
/// user's application password; it exists in memory and in the
/// settings file, and nowhere else.
pub struct WpSettings {
    pub site: String,
    pub user: String,
    pub password: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FileOutcome {
    Created,
    Updated,
    Failed(String),
}

#[derive(Clone)]
pub struct FileResult {
    pub path: String,
    pub outcome: FileOutcome,
    pub post_id: Option<u64>,
    /// The post's permalink, when WordPress gave one back.
    pub link: Option<String>,
}

/// One sync run, recorded with the bundle.
#[derive(Clone)]
pub struct SyncReport {
    pub at: u64,
    pub site: String,
    pub results: Vec<FileResult>,
    /// Files whose mapping was dropped because they left the
    /// publish set. Their WordPress posts were left as they are.
    pub dropped: Vec<String>,
}

impl SyncReport {
    pub fn to_json(&self) -> String {
        let results: Vec<String> = self
            .results
            .iter()
            .map(|r| {
                let (outcome, error) = match &r.outcome {
                    FileOutcome::Created => ("created", None),
                    FileOutcome::Updated => ("updated", None),
                    FileOutcome::Failed(msg) => ("failed", Some(msg.clone())),
                };
                let post_id = r
                    .post_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "null".to_string());
                let link = r
                    .link
                    .as_ref()
                    .map(|l| format!("\"{}\"", json_escape(l)))
                    .unwrap_or_else(|| "null".to_string());
                let error = error
                    .map(|e| format!(",\"error\":\"{}\"", json_escape(&e)))
                    .unwrap_or_default();
                format!(
                    "{{\"path\":\"{}\",\"outcome\":\"{outcome}\",\"post_id\":{post_id},\"link\":{link}{error}}}",
                    json_escape(&r.path)
                )
            })
            .collect();
        let dropped: Vec<String> = self
            .dropped
            .iter()
            .map(|p| format!("\"{}\"", json_escape(p)))
            .collect();
        format!(
            "{{\"site\":\"{}\",\"at\":{},\"results\":[{}],\"dropped\":[{}]}}",
            json_escape(&self.site),
            self.at,
            results.join(","),
            dropped.join(",")
        )
    }
}

/// The bundle's sync bookkeeping: the path -> post ID mapping, and
/// the last run's report.
#[derive(Default)]
pub struct WpState {
    pub map: HashMap<String, u64>,
    pub last: Option<SyncReport>,
}

/// Persistence for the per-bundle WordPress files, under the same
/// data directory the store owns. Writes follow the store's
/// durability shape: stage beside the home, sync, rename.
pub struct WpStore {
    dir: PathBuf,
}

impl WpStore {
    pub fn new(dir: PathBuf) -> WpStore {
        WpStore { dir }
    }

    fn slug_dir(&self, slug: &str) -> PathBuf {
        self.dir.join(slug)
    }

    // ——— settings ———

    pub fn load_settings(&self, slug: &str) -> Option<WpSettings> {
        let text = fs::read_to_string(self.slug_dir(slug).join("wordpress.txt")).ok()?;
        let mut site = None;
        let mut user = None;
        let mut password = None;
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("site=") {
                site = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("user=") {
                user = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("password=") {
                // Not trimmed beyond the line itself: an application
                // password may carry meaningful grouping spaces, and
                // WordPress ignores them either way.
                password = Some(v.to_string());
            }
        }
        match (site, user, password) {
            (Some(site), Some(user), Some(password))
                if !site.is_empty() && !user.is_empty() && !password.is_empty() =>
            {
                Some(WpSettings {
                    site,
                    user,
                    password,
                })
            }
            _ => None,
        }
    }

    pub fn save_settings(&self, slug: &str, settings: &WpSettings) -> std::io::Result<()> {
        let dir = self.slug_dir(slug);
        fs::create_dir_all(&dir)?;
        let text = format!(
            "site={}\nuser={}\npassword={}\n",
            settings.site, settings.user, settings.password
        );
        let path = dir.join("wordpress.txt");
        write_synced_private(&path, text.as_bytes())
    }

    /// Forget the connection and the bookkeeping both. The posts on
    /// WordPress are untouched — disconnecting is a local act.
    pub fn clear(&self, slug: &str) -> std::io::Result<()> {
        let dir = self.slug_dir(slug);
        for name in ["wordpress.txt", "wp-sync.txt"] {
            match fs::remove_file(dir.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    // ——— sync state ———

    pub fn load_state(&self, slug: &str) -> WpState {
        let mut state = WpState::default();
        let Ok(text) = fs::read_to_string(self.slug_dir(slug).join("wp-sync.txt")) else {
            return state;
        };
        let mut last_at = 0u64;
        let mut last_site = String::new();
        let mut results: Vec<FileResult> = Vec::new();
        let mut dropped: Vec<String> = Vec::new();
        let mut saw_last = false;
        for line in text.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            match parts.as_slice() {
                ["map", id, path] => {
                    if let Ok(id) = id.parse::<u64>() {
                        state.map.insert(path.to_string(), id);
                    }
                }
                ["site", site] => last_site = site.to_string(),
                ["last_at", at] => {
                    last_at = at.parse().unwrap_or(0);
                    saw_last = true;
                }
                ["last", outcome, post_id, path, detail] => {
                    let outcome = match *outcome {
                        "created" => FileOutcome::Created,
                        "updated" => FileOutcome::Updated,
                        _ => FileOutcome::Failed(detail.to_string()),
                    };
                    let (post_id, link) = match outcome {
                        FileOutcome::Failed(_) => (post_id.parse().ok(), None),
                        _ => (
                            post_id.parse().ok(),
                            if detail.is_empty() {
                                None
                            } else {
                                Some(detail.to_string())
                            },
                        ),
                    };
                    results.push(FileResult {
                        path: path.to_string(),
                        outcome,
                        post_id,
                        link,
                    });
                }
                ["dropped", path] => dropped.push(path.to_string()),
                _ => {}
            }
        }
        if saw_last {
            state.last = Some(SyncReport {
                at: last_at,
                site: last_site,
                results,
                dropped,
            });
        }
        state
    }

    pub fn save_state(&self, slug: &str, state: &WpState) -> std::io::Result<()> {
        let dir = self.slug_dir(slug);
        fs::create_dir_all(&dir)?;
        let mut text = String::new();
        let mut paths: Vec<&String> = state.map.keys().collect();
        paths.sort();
        for path in paths {
            text.push_str(&format!("map\t{}\t{}\n", state.map[path], path));
        }
        if let Some(last) = &state.last {
            text.push_str(&format!("site\t{}\n", one_line(&last.site)));
            text.push_str(&format!("last_at\t{}\n", last.at));
            for r in &last.results {
                let (outcome, detail) = match &r.outcome {
                    FileOutcome::Created => {
                        ("created", r.link.clone().unwrap_or_default())
                    }
                    FileOutcome::Updated => {
                        ("updated", r.link.clone().unwrap_or_default())
                    }
                    FileOutcome::Failed(msg) => ("failed", msg.clone()),
                };
                let post_id = r
                    .post_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "-".to_string());
                text.push_str(&format!(
                    "last\t{outcome}\t{post_id}\t{}\t{}\n",
                    r.path,
                    one_line(&detail)
                ));
            }
            for path in &last.dropped {
                text.push_str(&format!("dropped\t{}\n", path));
            }
        }
        write_synced(&dir.join("wp-sync.txt"), text.as_bytes())
    }
}

/// One line, printable, capped — the same discipline as the store's
/// change line, for anything recorded in a line-shaped file.
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(300)
        .collect::<String>()
        .trim()
        .to_string()
}

fn nonce() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", nonce()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// The settings file holds a credential: owner-only, at creation
/// and after every rewrite.
fn write_synced_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", nonce()));
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

// ——— settings validation ———

/// Is this host WordPress.com's? Their sites take the OAuth2 door,
/// which this build does not walk.
fn is_wordpress_com_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "wordpress.com" || host.ends_with(".wordpress.com")
}

/// Normalize and vet a site address the owner typed. Returns the
/// address to store (no trailing slash) or the plain-language reason
/// it cannot be used. Plain HTTP is accepted only for a site on this
/// machine — an application password must not cross the network in
/// the clear.
pub fn clean_site_url(raw: &str) -> Result<String, String> {
    let s = raw.trim().trim_end_matches('/');
    let (scheme, rest) = match s.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => ("http", rest),
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("https") => ("https", rest),
        _ => {
            return Err(
                "the site address needs to start with https:// (or http:// for a site on this machine)"
                    .to_string(),
            )
        }
    };
    let authority = rest.split('/').next().unwrap_or("");
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .trim_matches(|c| c == '[' || c == ']');
    if host.is_empty() {
        return Err("the site address has no host in it".to_string());
    }
    if is_wordpress_com_host(host) {
        return Err(
            "WordPress.com-hosted sites connect a different way (an OAuth2 sign-in), which isn't supported yet — this connection is for self-hosted WordPress"
                .to_string(),
        );
    }
    let local = {
        let h = host.to_ascii_lowercase();
        h == "localhost" || h == "127.0.0.1" || h == "::1"
    };
    if scheme == "http" && !local {
        return Err(
            "that address is http, not https — an application password must not cross the network unencrypted. Use the site's https address"
                .to_string(),
        );
    }
    Ok(s.to_string())
}

pub fn clean_user(raw: &str) -> Result<String, String> {
    let user = raw.trim().to_string();
    if user.is_empty() || user.chars().any(|c| c.is_control()) {
        return Err("a WordPress username is needed".to_string());
    }
    Ok(user)
}

// ——— the push ———

/// The files a publish sends to WordPress: the bundle's markdown,
/// in bundle order, the vault excluded (bundle order already
/// excludes it). Carried files (images and the like) are not posts.
pub fn publishable_markdown(bundle: &Bundle) -> Vec<(String, String, String)> {
    // (path, derived title, rendered HTML)
    ordered_files(&bundle.parsed)
        .into_iter()
        .filter(|f| f.is_markdown)
        .filter_map(|f| {
            let text = f.text()?;
            let title = incipit(text)
                .or_else(|| markdown_stem(&f.path))
                .unwrap_or_else(|| f.path.clone());
            Some((f.path.clone(), title, render_markdown(text)))
        })
        .collect()
}

/// `essays/one.md` -> `essays-one`: the whole stem, dashes for
/// slashes, so two files in different folders never collide on a
/// WordPress slug.
pub fn wp_slug_for(path: &str) -> String {
    let stem = markdown_stem(path).unwrap_or_else(|| path.to_string());
    stem.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Sync a bundle to its WordPress site, if one is connected.
/// Returns the report, which is also persisted with the state.
/// The mapping is saved after every file WordPress answers for, so
/// a crash mid-sync cannot make the next run create a duplicate of
/// a post this run already created.
pub fn sync_bundle(wp: &WpStore, bundle: &Bundle, settings: &WpSettings) -> SyncReport {
    let mut state = wp.load_state(&bundle.slug);
    let files = publishable_markdown(bundle);

    // Files that left the publish set lose their mapping. Their
    // posts stay on WordPress exactly as they are.
    let live: std::collections::HashSet<&str> =
        files.iter().map(|(p, _, _)| p.as_str()).collect();
    let mut dropped: Vec<String> = state
        .map
        .keys()
        .filter(|p| !live.contains(p.as_str()))
        .cloned()
        .collect();
    dropped.sort();
    for path in &dropped {
        state.map.remove(path);
    }

    let agent = agent();
    let mut results: Vec<FileResult> = Vec::with_capacity(files.len());
    for (path, title, html) in &files {
        let slug = wp_slug_for(path);
        let existing = state.map.get(path).copied();
        let result = push_one(&agent, settings, path, existing, title, &slug, html);
        if let Some(id) = result.post_id {
            if matches!(result.outcome, FileOutcome::Created | FileOutcome::Updated) {
                state.map.insert(path.clone(), id);
            }
        }
        results.push(result);
        let _ = wp.save_state(&bundle.slug, &state);
    }

    let report = SyncReport {
        at: now_unix(),
        site: settings.site.clone(),
        results,
        dropped,
    };
    state.last = Some(report.clone());
    let _ = wp.save_state(&bundle.slug, &state);
    report
}

/// The push that rides a publish — the web release, an MCP
/// publish, and a restore all ride the same hook. A restore is a
/// publish of the older state: it changes what is published, and
/// WordPress follows the published state — the bundle is the
/// source of truth either way. Runs only when a connection is
/// saved for the bundle, and never fails the publish it rides:
/// the sync records its own failures per file.
pub fn sync_if_configured(wp: &WpStore, bundle: &Bundle) -> Option<SyncReport> {
    let settings = wp.load_settings(&bundle.slug)?;
    Some(sync_bundle(wp, bundle, &settings))
}

/// Push one file: update its mapped post, or create the post when
/// there is no mapping — or when the mapped post is gone from
/// WordPress (a 404 on update falls through to create, and the
/// mapping is rewritten to the new post).
fn push_one(
    agent: &ureq::Agent,
    settings: &WpSettings,
    path: &str,
    existing: Option<u64>,
    title: &str,
    wp_slug: &str,
    html: &str,
) -> FileResult {
    let result = |outcome, post_id, link| FileResult {
        path: path.to_string(),
        outcome,
        post_id,
        link,
    };
    let body = format!(
        "{{\"title\":\"{}\",\"content\":\"{}\",\"slug\":\"{}\",\"status\":\"publish\"}}",
        json_escape(title),
        json_escape(html),
        json_escape(wp_slug)
    );
    if let Some(id) = existing {
        match wp_request(agent, settings, &format!("posts/{id}"), &body) {
            Ok((200, resp)) => {
                return result(
                    FileOutcome::Updated,
                    top_level_u64(&resp, "id").or(Some(id)),
                    top_level_string(&resp, "link"),
                );
            }
            Ok((404, _)) => {} // the post is gone; create it below
            Ok((code, resp)) => {
                return result(FileOutcome::Failed(wp_error(code, &resp)), Some(id), None);
            }
            Err(msg) => {
                return result(FileOutcome::Failed(msg), Some(id), None);
            }
        }
    }
    match wp_request(agent, settings, "posts", &body) {
        Ok((200 | 201, resp)) => match top_level_u64(&resp, "id") {
            Some(new_id) => result(
                FileOutcome::Created,
                Some(new_id),
                top_level_string(&resp, "link"),
            ),
            None => result(
                FileOutcome::Failed(
                    "WordPress answered success but named no post".to_string(),
                ),
                None,
                None,
            ),
        },
        Ok((code, resp)) => result(FileOutcome::Failed(wp_error(code, &resp)), None, None),
        Err(msg) => result(FileOutcome::Failed(msg), None, None),
    }
}

/// The site's own error words, when it sent any: WordPress error
/// bodies are JSON with a top-level `message`.
fn wp_error(code: u16, body: &str) -> String {
    match top_level_string(body, "message") {
        Some(msg) if !msg.trim().is_empty() => format!("WordPress answered {code}: {}", one_line(&msg)),
        _ => format!("WordPress answered {code}"),
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(30))
        .build()
}

/// One POST to the site's REST API. Ok carries (status, body) for
/// every HTTP answer, including errors — WordPress explains itself
/// in the body. Err is transport only: unreachable, TLS, timeout.
fn wp_request(
    agent: &ureq::Agent,
    settings: &WpSettings,
    route: &str,
    json_body: &str,
) -> Result<(u16, String), String> {
    let url = format!("{}/wp-json/wp/v2/{route}", settings.site);
    let auth = format!(
        "Basic {}",
        base64_encode(format!("{}:{}", settings.user, settings.password).as_bytes())
    );
    let resp = agent
        .post(&url)
        .set("Authorization", &auth)
        .set("Content-Type", "application/json")
        .send_string(json_body);
    match resp {
        Ok(r) => {
            let status = r.status();
            Ok((status, r.into_string().unwrap_or_default()))
        }
        Err(ureq::Error::Status(code, r)) => {
            Ok((code, r.into_string().unwrap_or_default()))
        }
        Err(ureq::Error::Transport(t)) => {
            Err(format!("could not reach WordPress: {}", one_line(&t.to_string())))
        }
    }
}

// ——— small JSON reading ———
//
// The codebase formats its JSON by hand and reads it the same way
// (see the e2e tests' helpers). WordPress answers are documents this
// code did not write, so the readers here scan properly: strings,
// escapes, and nesting depth, taking only top-level fields.

/// The raw token of a top-level field's value, if present.
fn top_level_raw<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let bytes = json.as_bytes();
    let mut i = 0usize;
    let mut depth = 0i32;
    while i < bytes.len() {
        match bytes[i] {
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                i += 1;
            }
            b'"' => {
                let (text, next) = scan_string(json, i)?;
                if depth == 1 && text == key {
                    // Expect ':' then the value.
                    let mut j = next;
                    while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b':' {
                        j += 1;
                        while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                            j += 1;
                        }
                        return Some(scan_value(json, j));
                    }
                }
                i = next;
            }
            _ => i += 1,
        }
    }
    None
}

/// Scan a JSON string starting at `start` (the opening quote).
/// Returns the unescaped text and the index just past the close.
fn scan_string(json: &str, start: usize) -> Option<(String, usize)> {
    let bytes = json.as_bytes();
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut out = String::new();
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some((out, i + 1)),
            b'\\' => {
                let esc = *bytes.get(i + 1)?;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000C}'),
                    b'u' => {
                        let hex = json.get(i + 2..i + 6)?;
                        let code = u32::from_str_radix(hex, 16).ok()?;
                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        i += 4;
                    }
                    _ => return None,
                }
                i += 2;
            }
            _ => {
                // Copy one UTF-8 character whole.
                let ch_len = utf8_len(bytes[i]);
                out.push_str(&json[i..i + ch_len]);
                i += ch_len;
            }
        }
    }
    None
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// The raw slice of the value starting at `start`: a string token
/// (quotes included), a number/true/false/null run, or a balanced
/// object/array.
fn scan_value<'a>(json: &'a str, start: usize) -> &'a str {
    let bytes = json.as_bytes();
    if start >= bytes.len() {
        return "";
    }
    match bytes[start] {
        b'"' => match scan_string(json, start) {
            Some((_, next)) => &json[start..next],
            None => &json[start..],
        },
        b'{' | b'[' => {
            let mut depth = 0i32;
            let mut i = start;
            while i < bytes.len() {
                match bytes[i] {
                    b'"' => match scan_string(json, i) {
                        Some((_, next)) => i = next,
                        None => return &json[start..],
                    },
                    b'{' | b'[' => {
                        depth += 1;
                        i += 1;
                    }
                    b'}' | b']' => {
                        depth -= 1;
                        i += 1;
                        if depth == 0 {
                            return &json[start..i];
                        }
                    }
                    _ => i += 1,
                }
            }
            &json[start..]
        }
        _ => {
            let mut i = start;
            while i < bytes.len()
                && !matches!(bytes[i], b',' | b'}' | b']')
                && !(bytes[i] as char).is_whitespace()
            {
                i += 1;
            }
            &json[start..i]
        }
    }
}

/// A top-level string field, unescaped.
pub fn top_level_string(json: &str, key: &str) -> Option<String> {
    let raw = top_level_raw(json, key)?;
    scan_string(raw, 0).map(|(text, _)| text)
}

/// A top-level unsigned-integer field.
pub fn top_level_u64(json: &str, key: &str) -> Option<u64> {
    let raw = top_level_raw(json, key)?;
    raw.trim().parse().ok()
}

/// One string field from a flat JSON object — the shape of the
/// settings request body the editor sends. Same scanner; the body
/// is an object, so its fields are top-level by construction.
pub fn json_string_field(body: &str, key: &str) -> Option<String> {
    top_level_string(body, key)
}

// ——— base64 (for the Basic header) ———

pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0] as u32,
            *chunk.get(1).unwrap_or(&0) as u32,
            *chunk.get(2).unwrap_or(&0) as u32,
        ];
        let n = (b[0] << 16) | (b[1] << 8) | b[2];
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"user:pass"), "dXNlcjpwYXNz");
    }

    #[test]
    fn site_url_rules() {
        assert_eq!(
            clean_site_url("https://example.com/").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            clean_site_url("  https://example.com/blog/ ").unwrap(),
            "https://example.com/blog"
        );
        assert!(clean_site_url("http://127.0.0.1:8080").is_ok());
        assert!(clean_site_url("http://localhost/wp").is_ok());
        // Plain HTTP to a remote host would carry the password in the clear.
        let e = clean_site_url("http://example.com").unwrap_err();
        assert!(e.contains("https"), "{e}");
        // WordPress.com takes the other door.
        let e = clean_site_url("https://wordpress.com").unwrap_err();
        assert!(e.contains("isn't supported yet"), "{e}");
        let e = clean_site_url("https://mysite.wordpress.com").unwrap_err();
        assert!(e.contains("isn't supported yet"), "{e}");
        // Not an address at all.
        assert!(clean_site_url("example.com").is_err());
        assert!(clean_site_url("https://").is_err());
        assert!(clean_site_url("ftp://example.com").is_err());
    }

    #[test]
    fn wp_slugs() {
        assert_eq!(wp_slug_for("index.md"), "index");
        assert_eq!(wp_slug_for("essays/one.md"), "essays-one");
        assert_eq!(wp_slug_for("a/b/c.markdown"), "a-b-c");
        assert_eq!(wp_slug_for("Notes On Rain.md"), "notes-on-rain");
    }

    #[test]
    fn top_level_readers() {
        // A post-shaped body: the top-level id wins over a nested one.
        let post = r#"{"id":421,"date":"2026-11-15T00:00:00","guid":{"rendered":"x"},"meta":{"id":9},"link":"https://example.com/?p=421"}"#;
        assert_eq!(top_level_u64(post, "id"), Some(421));
        assert_eq!(
            top_level_string(post, "link").as_deref(),
            Some("https://example.com/?p=421")
        );
        // An error body: message with an escaped quote inside.
        let err = r#"{"code":"rest_forbidden","message":"Sorry, you \"cannot\" do that.","data":{"status":403}}"#;
        assert_eq!(
            top_level_string(err, "message").as_deref(),
            Some("Sorry, you \"cannot\" do that.")
        );
        assert_eq!(top_level_u64(err, "status"), None); // nested, not top-level
        // Absent fields stay absent.
        assert_eq!(top_level_string(post, "message"), None);
    }

    #[test]
    fn settings_and_state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let wp = WpStore::new(dir.path().to_path_buf());
        let slug = "round-trip";
        fs::create_dir_all(dir.path().join(slug)).unwrap();
        assert!(wp.load_settings(slug).is_none());
        let settings = WpSettings {
            site: "https://example.com".to_string(),
            user: "will".to_string(),
            password: "abcd efgh ijkl".to_string(),
        };
        wp.save_settings(slug, &settings).unwrap();
        let back = wp.load_settings(slug).unwrap();
        assert_eq!(back.site, settings.site);
        assert_eq!(back.user, settings.user);
        assert_eq!(back.password, settings.password);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(slug).join("wordpress.txt"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the settings file is owner-only");
        }
        let mut state = WpState::default();
        state.map.insert("essays/one.md".to_string(), 421);
        state.last = Some(SyncReport {
            at: 123,
            site: settings.site.clone(),
            results: vec![FileResult {
                path: "essays/one.md".to_string(),
                outcome: FileOutcome::Created,
                post_id: Some(421),
                link: Some("https://example.com/essays-one".to_string()),
            }],
            dropped: vec!["old.md".to_string()],
        });
        wp.save_state(slug, &state).unwrap();
        let back = wp.load_state(slug);
        assert_eq!(back.map.get("essays/one.md"), Some(&421));
        let last = back.last.unwrap();
        assert_eq!(last.results.len(), 1);
        assert_eq!(last.results[0].outcome, FileOutcome::Created);
        assert_eq!(last.dropped, vec!["old.md".to_string()]);
        wp.clear(slug).unwrap();
        assert!(wp.load_settings(slug).is_none());
    }
}
