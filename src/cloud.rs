//! Cloud sync: a published bundle, copied to storage its author
//! owns.
//!
//! The WordPress adapter's discipline, pointed at object storage:
//! the connection is per bundle and held by whoever holds the
//! bundle's edit key; the sync runs after the local publish commit
//! and can never fail it; per-file results are recorded with the
//! bundle and shown on the release screen, which offers a retry
//! that re-syncs without re-publishing. One engine, one semantics
//! set, three providers:
//!
//! * **Amazon S3** — PUT Object, path-style, signed with Signature
//!   Version 4 (implemented below over the `sha2` the service
//!   already carries, proven against AWS's published signing test
//!   vector). An optional endpoint field serves S3-compatible
//!   stores with the same code path.
//! * **Google Cloud Storage** — its XML API with HMAC keys, which
//!   is deliberately S3-shaped: the same SigV4 signer, the same
//!   path-style PUT, credential scope region `auto`, service `s3`,
//!   against storage.googleapis.com (or a set endpoint).
//! * **Microsoft Azure Blob Storage** — PUT Blob against the Blob
//!   service REST API, authorized with a Shared Key signature
//!   (HMAC-SHA256 over Azure's canonical string-to-sign, the
//!   account key base64-decoded first). With an endpoint set the
//!   addressing is path-style (the Azurite shape); without one it
//!   is the account's own host.
//!
//! The payload is the bundle's deployable tree — the outputs
//! build's static site (outputs.rs), read from the on-disk tree it
//! generates at publish: the rendered pages, the PDFs, the raw
//! sources at their paths, the feed, the bundle zip — minus the
//! tree's own `deploy.zip`, which is the tree zipped and would be
//! itself twice. Files are uploaded under the connection's prefix
//! with the content types a static host serves. Unchanged files
//! are skipped: the server keeps the SHA-256 it last uploaded
//! successfully for every path, in the bundle's own directory.
//! A file that leaves the bundle is NOT deleted from the bucket or
//! container; its copy stays. Nothing here sets or touches the
//! destination's ACL, policy, or website configuration — that
//! posture is the owner's.
//!
//! The transport is `ureq`, the tree's one HTTP client (the
//! WordPress adapter's call, for the same reason: TLS). The
//! credentials are stored server-side only, in the bundle's own
//! directory with owner-only permissions; the API reports a key ID
//! by its last four characters and never returns a secret; no
//! credential is logged, and none is part of the bundle, its
//! download, or its feed.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::bundle::sha256_hex;
use crate::http::json_escape;
use crate::store::Bundle;

/* ════════════════════════ providers ════════════════════════ */

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    S3,
    Azure,
    Gcs,
}

impl Provider {
    pub fn parse(s: &str) -> Option<Provider> {
        match s.trim().to_ascii_lowercase().as_str() {
            "s3" => Some(Provider::S3),
            "azure" => Some(Provider::Azure),
            "gcs" => Some(Provider::Gcs),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::S3 => "s3",
            Provider::Azure => "azure",
            Provider::Gcs => "gcs",
        }
    }
}

/* ════════════════════════ settings ════════════════════════ */

/// One bundle's cloud connection, all three providers' fields in
/// one flat record (the `cloud.txt` lines form); only the active
/// provider's fields are read by the engine. Credentials live in
/// `access_key_id` / `secret_access_key` (S3, GCS) and
/// `account_key` (Azure).
#[derive(Clone, Default)]
pub struct CloudSettings {
    pub provider: Provider,
    // S3 + GCS
    pub bucket: String,
    pub region: String, // S3 only; GCS signs with scope region "auto"
    pub access_key_id: String,
    pub secret_access_key: String,
    // Azure
    pub account: String,
    pub container: String,
    pub account_key: String,
    // shared
    pub prefix: String,
    pub endpoint: Option<String>,
}

impl Default for Provider {
    fn default() -> Self {
        Provider::S3
    }
}

impl CloudSettings {
    pub fn to_lines(&self) -> String {
        let mut out = format!(
            "provider={}\nbucket={}\nregion={}\nprefix={}\naccess_key_id={}\nsecret_access_key={}\naccount={}\ncontainer={}\naccount_key={}\n",
            self.provider.as_str(),
            self.bucket,
            self.region,
            self.prefix,
            self.access_key_id,
            self.secret_access_key,
            self.account,
            self.container,
            self.account_key,
        );
        if let Some(e) = &self.endpoint {
            out += &format!("endpoint={e}\n");
        }
        out
    }

    /// Parse a `cloud.txt`. Anything short of the active
    /// provider's complete connection is not a connection — the
    /// caller treats it as unconfigured.
    pub fn from_lines(text: &str) -> Option<CloudSettings> {
        let mut s = CloudSettings::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "provider" => {
                    if let Some(p) = Provider::parse(v) {
                        s.provider = p;
                    }
                }
                "bucket" => s.bucket = v.to_string(),
                "region" => s.region = v.to_string(),
                "prefix" => s.prefix = v.to_string(),
                "endpoint" => {
                    s.endpoint = if v.is_empty() { None } else { Some(v.to_string()) };
                }
                "access_key_id" => s.access_key_id = v.to_string(),
                "secret_access_key" => s.secret_access_key = v.to_string(),
                "account" => s.account = v.to_string(),
                "container" => s.container = v.to_string(),
                "account_key" => s.account_key = v.to_string(),
                _ => {}
            }
        }
        let complete = match s.provider {
            Provider::S3 => {
                !s.bucket.is_empty()
                    && !s.region.is_empty()
                    && !s.access_key_id.is_empty()
                    && !s.secret_access_key.is_empty()
            }
            Provider::Gcs => {
                !s.bucket.is_empty()
                    && !s.access_key_id.is_empty()
                    && !s.secret_access_key.is_empty()
            }
            Provider::Azure => {
                !s.account.is_empty() && !s.container.is_empty() && !s.account_key.is_empty()
            }
        };
        if !complete {
            return None;
        }
        s.prefix = normalize_prefix(&s.prefix);
        Some(s)
    }

    /// The identity of the destination the sync state belongs to.
    /// Confirmations recorded against any other destination are
    /// void: switching provider, bucket, container, or prefix
    /// starts the new destination from unconfirmed.
    pub fn destination_id(&self) -> String {
        match self.provider {
            Provider::S3 => format!("s3|{}|{}", self.bucket, self.prefix),
            Provider::Gcs => format!("gcs|{}|{}", self.bucket, self.prefix),
            Provider::Azure => {
                format!("azure|{}|{}|{}", self.account, self.container, self.prefix)
            }
        }
    }
}

/// A prefix is a folder: no leading slash, a trailing slash when
/// it names anything.
pub fn normalize_prefix(p: &str) -> String {
    let p = p.trim().trim_start_matches('/');
    if p.is_empty() || p.ends_with('/') {
        p.to_string()
    } else {
        format!("{p}/")
    }
}

/// Merge a settings request body (the same `key=value` lines as
/// `cloud.txt`) over the settings already held, and validate the
/// result. The plain fields are authoritative when their line is
/// present (an empty endpoint line means "the provider's own");
/// the credential lines replace only when non-empty — the editor
/// never receives a saved credential back, so a blank from it can
/// only mean "keep what is saved". Switching the provider line
/// switches which connection is active; the other providers'
/// stored fields stay in the record, unused.
pub fn merge_settings(
    existing: Option<&CloudSettings>,
    body: &str,
) -> Result<CloudSettings, String> {
    let mut s = existing.cloned().unwrap_or_default();
    for line in body.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "provider" => {
                if let Some(p) = Provider::parse(v) {
                    s.provider = p;
                }
            }
            "bucket" => s.bucket = v.to_string(),
            "region" => s.region = v.to_string(),
            "prefix" => s.prefix = v.to_string(),
            "endpoint" => {
                s.endpoint = if v.is_empty() { None } else { Some(v.to_string()) };
            }
            "access_key_id" => {
                if !v.is_empty() {
                    s.access_key_id = v.to_string();
                }
            }
            "secret_access_key" => {
                if !v.is_empty() {
                    s.secret_access_key = v.to_string();
                }
            }
            "account" => s.account = v.to_string(),
            "container" => s.container = v.to_string(),
            "account_key" => {
                if !v.is_empty() {
                    s.account_key = v.to_string();
                }
            }
            _ => {}
        }
    }
    s.prefix = normalize_prefix(&s.prefix);
    validate_settings(&s)?;
    Ok(s)
}

/// Plain-language validation; the messages are user-facing and the
/// settings API returns them verbatim.
pub fn validate_settings(s: &CloudSettings) -> Result<(), String> {
    let fields = [
        s.bucket.as_str(),
        s.region.as_str(),
        s.prefix.as_str(),
        s.access_key_id.as_str(),
        s.secret_access_key.as_str(),
        s.account.as_str(),
        s.container.as_str(),
        s.account_key.as_str(),
    ];
    if fields.iter().any(|f| f.chars().any(|c| c.is_control())) {
        return Err("settings may not contain control characters".to_string());
    }
    match s.provider {
        Provider::S3 | Provider::Gcs => {
            let b = &s.bucket;
            if b.is_empty()
                || b.len() < 3
                || b.len() > 63
                || !b
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
                || b.starts_with('.')
                || b.ends_with('.')
            {
                return Err(format!(
                    "the bucket name \"{b}\" is not a name {} accepts",
                    if s.provider == Provider::S3 { "S3" } else { "Google Cloud Storage" }
                ));
            }
            if s.provider == Provider::S3
                && (s.region.is_empty()
                    || !s
                        .region
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
            {
                return Err(format!("the region \"{}\" does not look like an AWS region", s.region));
            }
            if s.access_key_id.is_empty() || s.secret_access_key.is_empty() {
                return Err(
                    "both halves of the access key are needed — the key ID and its secret"
                        .to_string(),
                );
            }
        }
        Provider::Azure => {
            let a = &s.account;
            if a.len() < 3
                || a.len() > 24
                || !a.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            {
                return Err(format!(
                    "the storage account \"{a}\" is not a name Azure accepts (3–24 lowercase letters and digits)"
                ));
            }
            let c = &s.container;
            let first_ok = c.chars().next().map(|ch| ch.is_ascii_alphanumeric()).unwrap_or(false);
            if c.len() < 3
                || c.len() > 63
                || !first_ok
                || !c
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
                || c.contains("--")
            {
                return Err(format!(
                    "the container name \"{c}\" is not a name Azure accepts (3–63 lowercase letters, digits, and single hyphens)"
                ));
            }
            if s.account_key.is_empty() {
                return Err("the account key is needed".to_string());
            }
            if base64_decode(&s.account_key).is_none() {
                return Err(
                    "the account key is not valid base64 — Azure account keys are".to_string(),
                );
            }
        }
    }
    if let Some(e) = &s.endpoint {
        parse_endpoint(e)?;
    }
    Ok(())
}

/// The settings as the API answers them: everything the editor
/// needs to render the connection, and no secret — a key ID is
/// named by its last four characters only, and a secret's presence
/// is all that is ever said about it.
pub fn settings_json(settings: Option<&CloudSettings>, state: &SyncState) -> String {
    let Some(s) = settings else {
        return "{\"ok\":true,\"configured\":false}".to_string();
    };
    let last = match &state.last_report {
        Some(r) if state.dest == s.destination_id() => {
            format!(",\"last_sync\":{}", report_json(r))
        }
        _ => ",\"last_sync\":null".to_string(),
    };
    let endpoint = match &s.endpoint {
        Some(e) => format!("\"{}\"", json_escape(e)),
        None => "null".to_string(),
    };
    match s.provider {
        Provider::S3 | Provider::Gcs => {
            let last4: String = s
                .access_key_id
                .chars()
                .rev()
                .take(4)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            format!(
                "{{\"ok\":true,\"configured\":true,\"provider\":\"{}\",\"bucket\":\"{}\",\"region\":\"{}\",\"prefix\":\"{}\",\"endpoint\":{},\"access_key_id_last4\":\"{}\",\"has_secret\":true{}}}",
                s.provider.as_str(),
                json_escape(&s.bucket),
                json_escape(&s.region),
                json_escape(&s.prefix),
                endpoint,
                json_escape(&last4),
                last,
            )
        }
        Provider::Azure => {
            format!(
                "{{\"ok\":true,\"configured\":true,\"provider\":\"azure\",\"account\":\"{}\",\"container\":\"{}\",\"prefix\":\"{}\",\"endpoint\":{},\"has_account_key\":true{}}}",
                json_escape(&s.account),
                json_escape(&s.container),
                json_escape(&s.prefix),
                endpoint,
                last,
            )
        }
    }
}

/* ════════════════════════ the store ════════════════════════ */

/// The per-bundle cloud files, beside the bundle in the data dir:
/// `cloud.txt` (the connection — a credential file, written mode
/// 0600 by staged rename) and `cloud-state.txt` (the sync state).
/// The WordPress store's shape, for the same reasons: reads hit
/// the disk per call, so a settings change made through any door
/// is the settings the next publish uses.
pub struct CloudStore {
    dir: PathBuf,
}

impl CloudStore {
    pub fn new(dir: PathBuf) -> CloudStore {
        CloudStore { dir }
    }

    fn settings_path(&self, slug: &str) -> PathBuf {
        self.dir.join(slug).join("cloud.txt")
    }
    fn state_path(&self, slug: &str) -> PathBuf {
        self.dir.join(slug).join("cloud-state.txt")
    }

    pub fn load_settings(&self, slug: &str) -> Option<CloudSettings> {
        let text = fs::read_to_string(self.settings_path(slug)).ok()?;
        CloudSettings::from_lines(&text)
    }

    pub fn save_settings(&self, slug: &str, settings: &CloudSettings) -> Result<(), String> {
        write_synced_private(&self.settings_path(slug), settings.to_lines().as_bytes())
    }

    /// Disconnect: both files go — the state belongs to the
    /// connection it describes. The destination keeps everything
    /// it has; only the road to it is removed.
    pub fn clear(&self, slug: &str) -> Result<(), String> {
        for p in [self.settings_path(slug), self.state_path(slug)] {
            match fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("cannot remove {}: {e}", p.display())),
            }
        }
        Ok(())
    }

    pub fn load_state(&self, slug: &str) -> SyncState {
        match fs::read_to_string(self.state_path(slug)) {
            Ok(text) => SyncState::from_lines(&text),
            Err(_) => SyncState::default(),
        }
    }

    pub fn record(&self, slug: &str, state: &SyncState) -> Result<(), String> {
        crate::store::write_synced(&self.state_path(slug), state.to_lines().as_bytes())
            .map_err(|e| format!("cannot write the sync state: {e}"))
    }
}

/// A credential-bearing staged write: same durability profile as
/// the store's own writes, owner-only permissions on the file
/// that lands.
fn write_synced_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::write(&tmp, bytes).map_err(|e| format!("cannot stage {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    if let Ok(f) = fs::File::open(&tmp) {
        let _ = f.sync_all();
    }
    fs::rename(&tmp, path).map_err(|e| format!("cannot install {}: {e}", path.display()))?;
    if let Some(parent) = path.parent() {
        let _ = crate::store::sync_dir(parent);
    }
    Ok(())
}

/* ════════════════════════ sync state ════════════════════════ */

#[derive(Clone)]
pub struct ConfirmedFile {
    pub sha256: String,
    pub etag: String,
}

#[derive(Clone, Default)]
pub struct SyncState {
    /// The destination these confirmations belong to (see
    /// `CloudSettings::destination_id`).
    pub dest: String,
    pub confirmed: HashMap<String, ConfirmedFile>,
    pub last_report: Option<CloudReport>,
}

impl SyncState {
    /// The confirmations that count for this destination — none,
    /// if the state was recorded against another one.
    pub fn confirmed_for(&self, dest: &str) -> HashMap<String, ConfirmedFile> {
        if self.dest == dest {
            self.confirmed.clone()
        } else {
            HashMap::new()
        }
    }

    pub fn to_lines(&self) -> String {
        let mut out = format!("dest={}\n", self.dest);
        let mut paths: Vec<&String> = self.confirmed.keys().collect();
        paths.sort();
        for p in paths {
            let c = &self.confirmed[p];
            out += &format!("{}\t{}\t{}\n", c.sha256, c.etag, p);
        }
        if let Some(r) = &self.last_report {
            out += &format!("report\t{}\n", report_json(r));
        }
        out
    }

    pub fn from_lines(text: &str) -> SyncState {
        let mut st = SyncState::default();
        for line in text.lines() {
            if let Some(d) = line.strip_prefix("dest=") {
                st.dest = d.to_string();
            } else if let Some(json) = line.strip_prefix("report\t") {
                st.last_report = report_from_json(json);
            } else {
                let mut parts = line.splitn(3, '\t');
                if let (Some(sha), Some(etag), Some(path)) =
                    (parts.next(), parts.next(), parts.next())
                {
                    if sha.len() == 64 && !path.is_empty() {
                        st.confirmed.insert(
                            path.to_string(),
                            ConfirmedFile {
                                sha256: sha.to_string(),
                                etag: etag.to_string(),
                            },
                        );
                    }
                }
            }
        }
        st
    }
}

/* ════════════════════════ reports ════════════════════════ */

/// One failed file of a sync: its tree path and the provider's
/// error, in the provider's words. (Successes and skips are
/// counts on the report; failures are the per-file detail.)
#[derive(Clone)]
pub struct FileOutcome {
    pub path: String,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct CloudReport {
    pub provider: Provider,
    pub at: u64,
    pub uploaded: usize,
    pub skipped: usize,
    pub failed: usize,
    pub failures: Vec<FileOutcome>,
}

pub fn report_json(r: &CloudReport) -> String {
    let mut out = format!(
        "{{\"provider\":\"{}\",\"at\":{},\"uploaded\":{},\"skipped\":{},\"failed\":{},\"failures\":[",
        r.provider.as_str(),
        r.at,
        r.uploaded,
        r.skipped,
        r.failed,
    );
    for (i, f) in r.failures.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out += &format!(
            "{{\"path\":\"{}\",\"error\":\"{}\"}}",
            json_escape(&f.path),
            json_escape(f.error.as_deref().unwrap_or(""))
        );
    }
    out += "]}";
    out
}

/// The state file carries the last report as the JSON above; this
/// reads back the counts and failures (the per-file detail the
/// editor renders). Deliberately a small hand parse — the shape is
/// this module's own.
fn report_from_json(json: &str) -> Option<CloudReport> {
    fn num(json: &str, key: &str) -> Option<u64> {
        let pat = format!("\"{key}\":");
        let rest = json.split(&pat).nth(1)?;
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    }
    fn string_field(json: &str, key: &str) -> Option<String> {
        let pat = format!("\"{key}\":\"");
        let rest = json.split(&pat).nth(1)?;
        let mut out = String::new();
        let mut chars = rest.chars();
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => match chars.next()? {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    other => out.push(other),
                },
                c => out.push(c),
            }
        }
        Some(out)
    }
    let provider = string_field(json, "provider")
        .and_then(|p| Provider::parse(&p))
        .unwrap_or_default();
    let mut failures = Vec::new();
    if let Some(arr) = json.split("\"failures\":[").nth(1) {
        for obj in arr.split("},{") {
            if let (Some(p), Some(e)) = (string_field(obj, "path"), string_field(obj, "error")) {
                failures.push(FileOutcome {
                    path: p,
                    error: Some(e),
                });
            }
        }
    }
    Some(CloudReport {
        provider,
        at: num(json, "at")?,
        uploaded: num(json, "uploaded")? as usize,
        skipped: num(json, "skipped")? as usize,
        failed: num(json, "failed")? as usize,
        failures,
    })
}

/* ════════════════════════ the tree ════════════════════════ */

/// One file of the deployable tree: its path inside the tree, the
/// content type a static host serves it under, and its bytes.
pub struct TreeFile {
    pub path: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// The payload: the outputs build's static site for this bundle,
/// read from the on-disk tree it generates at publish (outputs.rs
/// is the one tree producer — this module only carries it). The
/// tree's own `deploy.zip` is skipped: it is the tree, zipped, and
/// would be itself twice. If the tree is missing or visibly
/// incomplete (a bundle that predates a sync), it is generated
/// first, the same regeneration Store::open performs.
pub fn site_tree(dir: &Path, bundle: &Bundle) -> Result<Vec<TreeFile>, String> {
    if crate::outputs::outputs_incomplete(dir, bundle) {
        crate::outputs::generate(dir, bundle)?;
    }
    let site = dir.join(&bundle.slug).join("site");
    if !site.is_dir() {
        return Err("the bundle's static site has not been generated".to_string());
    }
    let mut out = Vec::new();
    collect_tree(&site, &site, &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn collect_tree(root: &Path, dir: &Path, out: &mut Vec<TreeFile>) -> Result<(), String> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_tree(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel == "deploy.zip" {
                continue;
            }
            let bytes =
                fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            out.push(TreeFile {
                content_type: content_type_for(&rel).to_string(),
                path: rel,
                bytes,
            });
        }
    }
    Ok(())
}

pub fn content_type_for(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower == "feed.xml" || lower.ends_with("/feed.xml") {
        return "application/rss+xml";
    }
    match lower.rsplit('.').next().unwrap_or("") {
        "html" | "htm" => "text/html",
        "pdf" => "application/pdf",
        "md" | "markdown" => "text/markdown",
        "xml" => "application/xml",
        "txt" => "text/plain",
        "css" => "text/css",
        "js" => "text/javascript",
        "json" => "application/json",
        "zip" => "application/zip",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "webp" => "image/webp",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/* ════════════════════════ signing ════════════════════════ */

fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let hashed = sha256_bytes(key);
        k[..32].copy_from_slice(&hashed);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    outer.finalize().into()
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// (basic `YYYYMMDD`, full `YYYYMMDDTHHMMSSZ`) for a unix time,
/// from the rss module's civil-date arithmetic.
fn amz_dates(unix: u64) -> (String, String) {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    let (y, m, d) = crate::rss::civil_from_days(days);
    let basic = format!("{y:04}{m:02}{d:02}");
    let full = format!(
        "{basic}T{:02}{:02}{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    );
    (basic, full)
}

/// RFC 1123 GMT date — the `x-ms-date` Azure's Shared Key signs.
fn rfc1123_from_unix(unix: u64) -> String {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    let (y, m, d) = crate::rss::civil_from_days(days);
    // 1970-01-01 was a Thursday.
    let wd = WEEKDAYS[((days + 4).rem_euclid(7)) as usize];
    format!(
        "{wd}, {d:02} {} {y} {:02}:{:02}:{:02} GMT",
        MONTHS[(m - 1) as usize],
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Percent-encode one key segment (the unreserved set passes,
/// everything else is encoded; `/` is structure, never data here).
fn encode_segment(seg: &str) -> String {
    let mut out = String::new();
    for &b in seg.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out += &format!("%{b:02X}"),
        }
    }
    out
}

fn encode_key(key: &str) -> String {
    key.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Signature Version 4, per AWS's published algorithm — the same
/// signer serves S3 and the GCS XML API (whose HMAC keys are made
/// to be used exactly this way, with scope region `auto`).
/// `headers` are the (lowercase name, value) pairs to sign; host
/// must be among them. Returns the Authorization header value.
#[allow(clippy::too_many_arguments)]
fn sign(
    method: &str,
    canonical_uri: &str,
    query: &str,
    headers: &[(String, String)],
    payload_sha256: &str,
    access_key_id: &str,
    secret_access_key: &str,
    region: &str,
    service: &str,
    amz_date: &str,
) -> String {
    let mut sorted: Vec<&(String, String)> = headers.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers: String = sorted
        .iter()
        .map(|(n, v)| format!("{n}:{}\n", v.trim()))
        .collect();
    let signed_headers: Vec<&str> = sorted.iter().map(|(n, _)| n.as_str()).collect();
    let signed = signed_headers.join(";");
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{query}\n{canonical_headers}\n{signed}\n{payload_sha256}"
    );
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let k_date = hmac_sha256(
        format!("AWS4{secret_access_key}").as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex_lower(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={access_key_id}/{scope}, SignedHeaders={signed}, Signature={signature}"
    )
}

/* ════════════════════════ base64 ════════════════════════
   Azure account keys are base64, and its Shared Key signature is
   base64 — the one encoding the tree did not already carry. */

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0] as u32,
            *chunk.get(1).unwrap_or(&0) as u32,
            *chunk.get(2).unwrap_or(&0) as u32,
        ];
        let n = (b[0] << 16) | (b[1] << 8) | b[2];
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let clean: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if clean.is_empty() || clean.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::new();
    for chunk in clean.chunks(4) {
        let mut vals = [0u32; 4];
        let mut pad = 0;
        for (i, &c) in chunk.iter().enumerate() {
            if c == b'=' {
                pad += 1;
                vals[i] = 0;
            } else {
                if pad > 0 {
                    return None;
                }
                vals[i] = B64.iter().position(|&x| x == c)? as u32;
            }
        }
        if pad > 2 {
            return None;
        }
        let n = (vals[0] << 18) | (vals[1] << 12) | (vals[2] << 6) | vals[3];
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// Azure's Shared Key string-to-sign for a PUT Blob (service
/// version 2020-10-02): the verb, the empty header fields, the
/// content length and type, then the canonicalized x-ms headers,
/// then the canonicalized resource — which always begins with the
/// account name, in both addressing styles.
fn azure_string_to_sign(
    account: &str,
    container: &str,
    blob: &str,
    content_type: &str,
    content_len: usize,
    date: &str,
    version: &str,
) -> String {
    format!(
        "PUT\n\n\n{content_len}\n\n{content_type}\n\n\n\n\n\n\nx-ms-blob-type:BlockBlob\nx-ms-date:{date}\nx-ms-version:{version}\n/{account}/{container}/{blob}"
    )
}

fn azure_authorization(
    settings: &CloudSettings,
    blob: &str,
    content_type: &str,
    content_len: usize,
    date: &str,
    version: &str,
) -> Result<String, String> {
    let key = base64_decode(&settings.account_key)
        .ok_or_else(|| "the account key is not valid base64".to_string())?;
    let sts = azure_string_to_sign(
        &settings.account,
        &settings.container,
        blob,
        content_type,
        content_len,
        date,
        version,
    );
    let sig = base64_encode(&hmac_sha256(&key, sts.as_bytes()));
    Ok(format!("SharedKey {}:{sig}", settings.account))
}

/* ════════════════════════ transport ════════════════════════ */

/// Split an endpoint into (scheme, authority). The endpoint names
/// an origin only — bucket and key always ride the path
/// (path-style addressing, the one shape that serves AWS regions,
/// GCS, Azurite, and S3-compatible stores alike).
fn parse_endpoint(endpoint: &str) -> Result<(String, String), String> {
    let e = endpoint.trim().trim_end_matches('/');
    let Some((scheme, rest)) = e.split_once("://") else {
        return Err(format!(
            "the endpoint \"{endpoint}\" needs a scheme — https:// (or http:// for a local store)"
        ));
    };
    if scheme != "https" && scheme != "http" {
        return Err(format!("the endpoint scheme \"{scheme}\" is not http or https"));
    }
    if rest.is_empty() || rest.contains('/') || rest.contains('?') || rest.contains('#') {
        return Err(format!(
            "the endpoint \"{endpoint}\" should be an origin only — no path"
        ));
    }
    Ok((scheme.to_string(), rest.to_string()))
}

/// The provider's default origin (scheme, authority).
fn default_origin(s: &CloudSettings) -> (String, String) {
    match s.provider {
        Provider::S3 => (
            "https".to_string(),
            format!("s3.{}.amazonaws.com", s.region),
        ),
        Provider::Gcs => ("https".to_string(), "storage.googleapis.com".to_string()),
        Provider::Azure => (
            "https".to_string(),
            format!("{}.blob.core.windows.net", s.account),
        ),
    }
}

fn xml_element(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml_unescape(&xml[start..end]))
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The tree's one HTTP client, with the WordPress adapter's
/// timeouts.
fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(30))
        .build()
}

/// PUT one object to S3 or GCS (the SigV4 pair). Returns the ETag
/// the store answered with, or its own error in its own words.
fn put_sigv4(
    agent: &ureq::Agent,
    s: &CloudSettings,
    key: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<String, String> {
    let (scheme, authority) = match &s.endpoint {
        Some(e) => parse_endpoint(e)?,
        None => default_origin(s),
    };
    let region = if s.provider == Provider::Gcs {
        "auto"
    } else {
        s.region.as_str()
    };
    let url = format!("{scheme}://{authority}/{}/{}", s.bucket, encode_key(key));
    let canonical_uri = format!("/{}/{}", encode_segment(&s.bucket), encode_key(key));
    let (_, amz_date) = amz_dates(now_unix());
    let payload_sha = sha256_hex(bytes);
    let headers: Vec<(String, String)> = vec![
        ("content-type".into(), content_type.to_string()),
        ("host".into(), authority.clone()),
        ("x-amz-content-sha256".into(), payload_sha.clone()),
        ("x-amz-date".into(), amz_date.clone()),
    ];
    let authorization = sign(
        "PUT",
        &canonical_uri,
        "",
        &headers,
        &payload_sha,
        &s.access_key_id,
        &s.secret_access_key,
        region,
        "s3",
        &amz_date,
    );
    let resp = agent
        .put(&url)
        .set("Content-Type", content_type)
        .set("x-amz-content-sha256", &payload_sha)
        .set("x-amz-date", &amz_date)
        .set("Authorization", &authorization)
        .send_bytes(bytes);
    match resp {
        Ok(r) => Ok(r.header("ETag").unwrap_or("").trim_matches('"').to_string()),
        Err(ureq::Error::Status(_, r)) => {
            let body = r.into_string().unwrap_or_default();
            let code = xml_element(&body, "Code").unwrap_or_else(|| "UnknownError".to_string());
            let msg = xml_element(&body, "Message").unwrap_or_else(|| one_line(&body));
            Err(format!("{code}: {msg}"))
        }
        Err(e) => Err(format!("could not reach the endpoint: {e}")),
    }
}

const AZURE_VERSION: &str = "2020-10-02";

/// PUT one blob to Azure. Returns the ETag, or Azure's own error.
fn put_azure(
    agent: &ureq::Agent,
    s: &CloudSettings,
    blob: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<String, String> {
    let date = rfc1123_from_unix(now_unix());
    let authorization = azure_authorization(s, blob, content_type, bytes.len(), &date, AZURE_VERSION)?;
    let url = match &s.endpoint {
        // Path-style (the Azurite shape): the account rides the
        // path; the canonicalized resource is the same either way.
        Some(e) => {
            let (scheme, authority) = parse_endpoint(e)?;
            format!(
                "{scheme}://{authority}/{}/{}/{}",
                s.account,
                s.container,
                encode_key(blob)
            )
        }
        None => format!(
            "https://{}.blob.core.windows.net/{}/{}",
            s.account,
            s.container,
            encode_key(blob)
        ),
    };
    let resp = agent
        .put(&url)
        .set("Content-Type", content_type)
        .set("x-ms-date", &date)
        .set("x-ms-version", AZURE_VERSION)
        .set("x-ms-blob-type", "BlockBlob")
        .set("Authorization", &authorization)
        .send_bytes(bytes);
    match resp {
        Ok(r) => Ok(r.header("ETag").unwrap_or("").trim_matches('"').to_string()),
        Err(ureq::Error::Status(_, r)) => {
            let body = r.into_string().unwrap_or_default();
            let code = xml_element(&body, "Code").unwrap_or_else(|| "UnknownError".to_string());
            let msg = xml_element(&body, "Message").unwrap_or_else(|| one_line(&body));
            Err(format!("{code}: {msg}"))
        }
        Err(e) => Err(format!("could not reach the endpoint: {e}")),
    }
}

fn put_object(
    agent: &ureq::Agent,
    s: &CloudSettings,
    key: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<String, String> {
    match s.provider {
        Provider::S3 | Provider::Gcs => put_sigv4(agent, s, key, content_type, bytes),
        Provider::Azure => put_azure(agent, s, key, content_type, bytes),
    }
}

/* ════════════════════════ the engine ════════════════════════ */

/// Sync one bundle's tree to its connection's destination. Every
/// file is answered for: uploaded, skipped (its content hash is
/// the one the state last confirmed for this destination), or
/// failed with the provider's error. The report is recorded in
/// the sync state either way; a failed file keeps its previous
/// confirmation, so the next sync retries exactly it and its
/// changed kin.
fn run_sync(store: &CloudStore, bundle: &Bundle, s: &CloudSettings) -> Result<CloudReport, String> {
    let tree = site_tree(&store.dir, bundle)?;
    let dest = s.destination_id();
    let mut state = store.load_state(&bundle.slug);
    let mut confirmed = state.confirmed_for(&dest);
    let agent = agent();
    let mut report = CloudReport {
        provider: s.provider,
        at: now_unix(),
        uploaded: 0,
        skipped: 0,
        failed: 0,
        failures: Vec::new(),
    };
    for file in &tree {
        let key = format!("{}{}", s.prefix, file.path);
        let hash = sha256_hex(&file.bytes);
        if confirmed.get(&file.path).map(|c| c.sha256 == hash).unwrap_or(false) {
            report.skipped += 1;
            continue;
        }
        match put_object(&agent, s, &key, &file.content_type, &file.bytes) {
            Ok(etag) => {
                confirmed.insert(
                    file.path.clone(),
                    ConfirmedFile {
                        sha256: hash,
                        etag,
                    },
                );
                report.uploaded += 1;
            }
            Err(error) => {
                report.failed += 1;
                report.failures.push(FileOutcome {
                    path: file.path.clone(),
                    error: Some(error),
                });
            }
        }
    }
    state.dest = dest;
    state.confirmed = confirmed;
    state.last_report = Some(report.clone());
    if let Err(e) = store.record(&bundle.slug, &state) {
        eprintln!(
            "markdown-wizard: cannot record cloud sync state for {}: {e}",
            bundle.slug
        );
    }
    Ok(report)
}

/// The publish hook, in the WordPress adapter's shape: when the
/// bundle has a cloud connection, sync the just-committed bundle
/// and return the report; the publish itself has already landed
/// and is never touched by the outcome.
pub fn sync_if_configured(store: &CloudStore, bundle: &Bundle) -> Option<CloudReport> {
    let settings = store.load_settings(&bundle.slug)?;
    match run_sync(store, bundle, &settings) {
        Ok(report) => Some(report),
        Err(e) => {
            // The tree itself could not be read or generated — a
            // whole-sync failure, recorded as one failed entry so
            // the publish result says so instead of staying silent.
            let report = CloudReport {
                provider: settings.provider,
                at: now_unix(),
                uploaded: 0,
                skipped: 0,
                failed: 1,
                failures: vec![FileOutcome {
                    path: "(the static site)".to_string(),
                    error: Some(e),
                }],
            };
            let mut state = store.load_state(&bundle.slug);
            state.dest = settings.destination_id();
            state.last_report = Some(report.clone());
            let _ = store.record(&bundle.slug, &state);
            Some(report)
        }
    }
}

/// Sync on demand (the retry): the stored connection, the stored
/// bundle, no re-publish.
pub fn sync_bundle(
    store: &CloudStore,
    bundle: &Bundle,
    settings: &CloudSettings,
) -> Result<CloudReport, String> {
    run_sync(store, bundle, settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231() {
        // Test case 1: key = 20 x 0x0b, data = "Hi There".
        let out = hmac_sha256(&[0x0b; 20], b"Hi There");
        assert_eq!(
            hex_lower(&out),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Test case 2: key = "Jefe".
        let out = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex_lower(&out),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn sigv4_matches_aws_published_example() {
        // AWS's documented SigV4 example (IAM ListUsers, a GET
        // with an empty body — the parameters ride the query
        // string): the signature is known independently of this
        // code.
        let headers: Vec<(String, String)> = vec![
            ("content-type".into(), "application/x-www-form-urlencoded; charset=utf-8".into()),
            ("host".into(), "iam.amazonaws.com".into()),
            ("x-amz-date".into(), "20150830T123600Z".into()),
        ];
        let auth = sign(
            "GET",
            "/",
            "Action=ListUsers&Version=2010-05-08",
            &headers,
            &sha256_hex(b""),
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            "iam",
            "20150830T123600Z",
        );
        assert!(auth.contains(
            "Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        ), "{auth}");
    }

    #[test]
    fn dates_format() {
        assert_eq!(
            amz_dates(0),
            ("19700101".to_string(), "19700101T000000Z".to_string())
        );
        assert_eq!(rfc1123_from_unix(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(
            rfc1123_from_unix(1_794_744_000),
            "Sun, 15 Nov 2026 12:00:00 GMT"
        );
    }

    #[test]
    fn base64_matches_rfc4648() {
        let cases: [(&[u8], &str); 6] = [
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (raw, coded) in cases {
            assert_eq!(base64_encode(raw), coded);
            assert_eq!(base64_decode(coded).as_deref(), Some(raw));
        }
        assert_eq!(base64_decode("Zg="), None);
        assert_eq!(base64_decode("Z==="), None);
        assert_eq!(base64_decode("not base64!!"), None);
        assert_eq!(base64_decode(""), None);
    }

    #[test]
    fn azure_string_to_sign_is_exact() {
        // The canonical form, field by field: PUT, three empty
        // fields around the length, the type, five empty fields,
        // the x-ms headers, the resource.
        let sts = azure_string_to_sign(
            "myaccount",
            "mycontainer",
            "site/index.html",
            "text/html",
            42,
            "Sun, 15 Nov 2026 12:00:00 GMT",
            "2020-10-02",
        );
        assert_eq!(
            sts,
            "PUT\n\n\n42\n\ntext/html\n\n\n\n\n\n\nx-ms-blob-type:BlockBlob\nx-ms-date:Sun, 15 Nov 2026 12:00:00 GMT\nx-ms-version:2020-10-02\n/myaccount/mycontainer/site/index.html"
        );
    }

    #[test]
    fn prefixes_normalize() {
        assert_eq!(normalize_prefix(""), "");
        assert_eq!(normalize_prefix("site"), "site/");
        assert_eq!(normalize_prefix("/site/"), "site/");
        assert_eq!(normalize_prefix("a/b"), "a/b/");
    }

    #[test]
    fn endpoints_must_be_origins() {
        assert!(parse_endpoint("https://minio.local:9000").is_ok());
        assert!(parse_endpoint("https://example.com/some/path").is_err());
        assert!(parse_endpoint("example.com").is_err());
    }

    fn s3_body(extra: &str) -> String {
        format!(
            "provider=s3\nbucket=my-bucket\nregion=us-east-1\naccess_key_id=AKIDEXAMPLE\nsecret_access_key=first-secret\n{extra}"
        )
    }

    #[test]
    fn merge_keeps_saved_credentials_on_blank() {
        let first = merge_settings(None, &s3_body("")).unwrap();
        let second = merge_settings(
            Some(&first),
            "provider=s3\nbucket=my-bucket\nregion=eu-west-1\naccess_key_id=\nsecret_access_key=\n",
        )
        .unwrap();
        assert_eq!(second.region, "eu-west-1");
        assert_eq!(second.access_key_id, "AKIDEXAMPLE");
        assert_eq!(second.secret_access_key, "first-secret");
    }

    #[test]
    fn gcs_needs_no_region_and_azure_needs_base64_key() {
        let gcs = merge_settings(
            None,
            "provider=gcs\nbucket=my-bucket\naccess_key_id=HMACID\nsecret_access_key=s3cr3t\n",
        )
        .unwrap();
        assert_eq!(gcs.provider, Provider::Gcs);
        let key = base64_encode(&[7u8; 32]);
        let azure = merge_settings(
            None,
            &format!("provider=azure\naccount=myaccount\ncontainer=my-container\naccount_key={key}\n"),
        )
        .unwrap();
        assert_eq!(azure.provider, Provider::Azure);
        let bad = merge_settings(
            None,
            "provider=azure\naccount=myaccount\ncontainer=my-container\naccount_key=not!base64\n",
        );
        match bad {
            Ok(_) => panic!("a non-base64 account key was accepted"),
            Err(e) => assert!(e.contains("base64"), "{e}"),
        }
    }

    #[test]
    fn summaries_never_carry_a_secret() {
        let state = SyncState::default();
        for body in [
            s3_body(""),
            "provider=gcs\nbucket=my-bucket\naccess_key_id=HMACIDZZZZ\nsecret_access_key=gcs-secret\n".to_string(),
            format!(
                "provider=azure\naccount=myaccount\ncontainer=my-container\naccount_key={}\n",
                base64_encode(b"the-azure-account-key-bytes")
            ),
        ] {
            let s = merge_settings(None, &body).unwrap();
            let json = settings_json(Some(&s), &state);
            assert!(!json.contains("first-secret"), "{json}");
            assert!(!json.contains("gcs-secret"), "{json}");
            assert!(!json.contains("the-azure-account-key"), "{json}");
            assert!(json.contains("\"configured\":true"), "{json}");
        }
        let s = merge_settings(None, &s3_body("")).unwrap();
        let json = settings_json(Some(&s), &state);
        assert!(json.contains("\"access_key_id_last4\":\"MPLE\""), "{json}");
    }

    #[test]
    fn state_belongs_to_its_destination() {
        let mut st = SyncState::default();
        st.dest = "s3|one-bucket|".to_string();
        st.confirmed.insert(
            "index.html".to_string(),
            ConfirmedFile {
                sha256: "a".repeat(64),
                etag: "e".to_string(),
            },
        );
        assert_eq!(st.confirmed_for("s3|one-bucket|").len(), 1);
        assert!(st.confirmed_for("s3|other-bucket|").is_empty());
        assert!(st.confirmed_for("azure|acct|cont|").is_empty());
        // And it round-trips through its lines form.
        let back = SyncState::from_lines(&st.to_lines());
        assert_eq!(back.dest, st.dest);
        assert_eq!(back.confirmed.len(), 1);
    }

    #[test]
    fn content_types_cover_the_tree() {
        assert_eq!(content_type_for("index.html"), "text/html");
        assert_eq!(content_type_for("essays/one.pdf"), "application/pdf");
        assert_eq!(content_type_for("essays/one.md"), "text/markdown");
        assert_eq!(content_type_for("feed.xml"), "application/rss+xml");
        assert_eq!(content_type_for("README.txt"), "text/plain");
        assert_eq!(content_type_for("favicon.png"), "image/png");
        assert_eq!(content_type_for("mascot-badge.webp"), "image/webp");
        assert_eq!(content_type_for("bundle.zip"), "application/zip");
    }
}
