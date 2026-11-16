//! Federation — markdown-wizard as an OAuth 2.1 client of
//! edit.phext.io, and the phext vault that sign-in unlocks.
//!
//! What edit.phext.io offers (read in phext-lattice's `oauth.rs`
//! and `phext_edit.rs`, the provider's own source): an authorization
//! server with RFC 8414 discovery, RFC 7591 dynamic client
//! registration, authorization codes with mandatory PKCE S256,
//! rotating refresh tokens, RFC 8707 resource indicators, and a
//! stateless MCP endpoint whose tools (`list_phexts`, `read_scroll`,
//! `write_scroll`, …) work on the phexts it hosts. This module is a
//! client of that surface and nothing else — phext-lattice is not
//! modified, and nothing here talks to its editor-cookie API.
//!
//! **Sign-in is a door, never a gate.** Nothing in this module is
//! on the path of the signed-out editor: drafts stay local-first,
//! publishing stays key-based, and every handler here answers a
//! clean "not signed in" that the editor treats as a state, not an
//! error. Federation is off entirely with `MDWIZ_FEDERATION=off`.
//!
//! **Client registration** happens by DCR at the first sign-in and
//! is cached (per issuer + redirect URI) in `<data>/fed/state.json`.
//! The token-endpoint auth method is `none` — a public client —
//! with PKCE as the binding: the provider's own test suite mints
//! its tokens exactly this way, no credential material exists here
//! to store or rotate, and the choice walks around the
//! `private_key_jwt` assertion-verification path (the one that
//! refuses ChatGPT's client on the provider side) entirely. The
//! code verifier never leaves this server's memory; the tokens the
//! exchange returns are held server-side only, behind an HttpOnly
//! session cookie — the browser never sees an edit.phext.io token.
//!
//! **The phext vault.** A signed-in user's drafts can live in one
//! phext on edit.phext.io, named `mdwiz-vault-<id>` (see
//! `new_vault_name`), written with the user's own access token —
//! the vault is the user's phext under the user's identity, not a
//! service account's. Inside it (a scroll is a 2D page of text;
//! each manifest is one contiguous scroll):
//!
//!   1.1.1/1.1.1/1.1.1   the vault manifest: format, name, label,
//!                       and one line per bundle (slug, number,
//!                       revision, last update)
//!   1.2.n/1.1.1/1.1.1   bundle n's manifest: one line per file
//!                       (Shelf 2 — no bundle number can collide
//!                       with the vault manifest at the root)
//!                       (number, revision, SHA-256, path)
//!   2.1.n/1.1.m/1.1.1   file m of bundle n: the markdown itself
//!
//! Two provider limits shape this, and the README states both:
//! the OAuth surface has **no userinfo endpoint** (a token knows
//! its account server-side; the client cannot learn the name), so
//! the editor shows the vault's own label where a name would go;
//! and MCP can only write phexts edit.phext.io already **hosts** —
//! there is no create call — so a new vault begins as a starter
//! `.phext` file the user hosts (the `/api/vault/starter`
//! download, submittable at edit.phext.io/submit), and "not hosted
//! yet" is a first-class state of the vault endpoints, not a
//! failure.
//!
//! **Sync** is per file and revision-based: the browser remembers
//! the (revision, hash) it last synced at; a push writes files
//! that changed locally while the vault stood still, reports
//! files the vault moved alone, and names a **conflict** — never
//! an overwrite — when both sides changed the same file. The
//! editor resolves conflicts per file (keep mine / take the
//! vault's). Text files only: a binary file is reported as not
//! carried, never silently dropped.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::Digest;

use crate::bundle::{sha256_hex, valid_bundle_path, MAX_FILES, MAX_FILE_BYTES};
use crate::http::{self, CacheClass, HTML, JSON, TEXT};
use std::collections::HashMap as Map;
use std::io::BufReader;
use std::net::TcpStream;

pub const DEFAULT_ISSUER: &str = "https://edit.phext.io";
pub const VAULT_PREFIX: &str = "mdwiz-vault-";
const SCOPES: &str = "phext:read phext:write";
const SESSION_COOKIE: &str = "mdwiz_session";
const SESSION_MAX_AGE: i64 = 30 * 86_400;
const FLOW_TTL: u64 = 600;
const VAULT_FORMAT: &str = "markdown-wizard vault v1";
const BUNDLE_FORMAT: &str = "markdown-wizard bundle v1";
const MAX_VAULT_BUNDLES: usize = 200;

// ── configuration & state ────────────────────────────────────────

pub struct FedConfig {
    pub enabled: bool,
    pub issuer: String,
}

impl FedConfig {
    pub fn from_env() -> FedConfig {
        let enabled = std::env::var("MDWIZ_FEDERATION")
            .map(|v| !v.eq_ignore_ascii_case("off") && v != "0")
            .unwrap_or(true);
        let issuer = std::env::var("MDWIZ_PHEXT_ISSUER")
            .ok()
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_ISSUER.to_string());
        FedConfig { enabled, issuer }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Registration {
    issuer: String,
    redirect_uri: String,
    client_id: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Session {
    access_token: String,
    refresh_token: String,
    /// Unix seconds when the access token stops working.
    expires_at: u64,
    #[serde(default)]
    vault_name: Option<String>,
    #[serde(default)]
    vault_label: Option<String>,
    created_at: u64,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct FedState {
    #[serde(default)]
    registration: Option<Registration>,
    #[serde(default)]
    sessions: HashMap<String, Session>,
}

struct PendingFlow {
    verifier: String,
    redirect_uri: String,
    created_at: u64,
}

pub struct Fed {
    cfg: FedConfig,
    state_path: PathBuf,
    state: Mutex<FedState>,
    pending: Mutex<HashMap<String, PendingFlow>>,
    discovery: Mutex<Option<(String, Discovery)>>,
    /// Serializes token refreshes (they rotate; two at once would
    /// invalidate each other) and whole vault syncs (the manifests
    /// are read-modify-write).
    token_lock: Mutex<()>,
    vault_lock: Mutex<()>,
    agent: ureq::Agent,
}

#[derive(Clone)]
struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    resource: String,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Random bytes from the kernel, hex — the same source and shape
/// as the bundle edit keys (bundle.rs).
fn random_hex(bytes: usize) -> std::io::Result<String> {
    let mut buf = vec![0u8; bytes];
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(&mut buf)?;
    let mut out = String::with_capacity(bytes * 2);
    for b in buf {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    Ok(out)
}

/// base64url without padding (RFC 4648 §5) — PKCE challenges and
/// nothing else here needs base64 at all.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0] as u32,
            *chunk.get(1).unwrap_or(&0) as u32,
            *chunk.get(2).unwrap_or(&0) as u32,
        ];
        let n = (b[0] << 16) | (b[1] << 8) | b[2];
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

/// The PKCE S256 challenge for a verifier.
fn pkce_challenge(verifier: &str) -> String {
    base64url(&sha2::Sha256::digest(verifier.as_bytes()))
}

/// Percent-encode one form/query value (unreserved characters
/// pass, everything else is %XX; a space is %20, never `+`, so the
/// value survives both query parsers and form parsers).
fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn form_pairs(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Parse a query string or form body into pairs (form rules: `+`
/// is a space here, unlike in a path).
fn parse_pairs(raw: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let decode = |s: &str| http::percent_decode(&s.replace('+', " "));
        out.insert(decode(k), decode(v));
    }
    out
}

/// One line of user text, printable and capped — vault labels ride
/// in manifests and page titles, so they get the change-line
/// discipline (compare store::clean_changed_line).
fn clean_label(raw: &str) -> String {
    // Control characters become spaces (a newline inside a label
    // must never fuse two words together, nor break the manifest
    // line the label rides on), and whitespace runs collapse.
    let spaced: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(80)
        .collect();
    let cleaned = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        "My Markdown Wizard vault".to_string()
    } else {
        cleaned
    }
}

impl Fed {
    /// Open the federation state under `<data>/fed/`. A torn or
    /// unreadable state file is not fatal: federation starts empty
    /// (everyone signs in again) and says so on stderr — the rest
    /// of the service never depends on this state.
    pub fn open(data_dir: &std::path::Path, cfg: FedConfig) -> std::io::Result<Fed> {
        let dir = data_dir.join("fed");
        std::fs::create_dir_all(&dir)?;
        let state_path = dir.join("state.json");
        let state = match std::fs::read(&state_path) {
            Ok(bytes) => serde_json::from_slice::<FedState>(&bytes).unwrap_or_else(|e| {
                eprintln!("markdown-wizard: fed state unreadable ({e}); starting signed-out");
                FedState::default()
            }),
            Err(_) => FedState::default(),
        };
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(10))
            .timeout_read(std::time::Duration::from_secs(30))
            .build();
        Ok(Fed {
            cfg,
            state_path,
            state: Mutex::new(state),
            pending: Mutex::new(HashMap::new()),
            discovery: Mutex::new(None),
            token_lock: Mutex::new(()),
            vault_lock: Mutex::new(()),
            agent,
        })
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn issuer(&self) -> &str {
        &self.cfg.issuer
    }

    /// Persist the state file: stage, sync, rename, 0600 — it
    /// holds live bearer tokens, so it is written like a secret
    /// and never world-readable. The staged file is created with
    /// its mode, not chmodded after the fact, so the tokens never
    /// sit at default permissions, even briefly (the WordPress
    /// store's write_synced_private sets the pattern).
    fn persist(&self, state: &FedState) {
        let Ok(text) = serde_json::to_string(state) else {
            return;
        };
        let tmp = self.state_path.with_extension("json.tmp");
        if write_private_synced(&tmp, text.as_bytes()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.state_path);
        }
    }

    fn session_id(&self, headers: &Map<String, String>) -> Option<String> {
        let cookie = headers.get("cookie")?;
        for part in cookie.split(';') {
            let part = part.trim();
            if let Some(v) = part.strip_prefix(&format!("{SESSION_COOKIE}=")) {
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
        None
    }

    fn session(&self, headers: &Map<String, String>) -> Option<(String, Session)> {
        let sid = self.session_id(headers)?;
        let mut state = self.state.lock().unwrap();
        // A session lives SESSION_MAX_AGE from its creation — the
        // same age the cookie carries. The stored record enforces
        // it too: a session id copied out of the browser must not
        // outlive the cookie that was meant to bound it, riding
        // refresh rotations forever.
        let expired = state
            .sessions
            .get(&sid)
            .is_some_and(|s| now_unix().saturating_sub(s.created_at) > SESSION_MAX_AGE as u64);
        if expired {
            state.sessions.remove(&sid);
            self.persist(&state);
            return None;
        }
        state.sessions.get(&sid).cloned().map(|s| (sid, s))
    }
}

/// Write bytes to a fresh file with owner-only permissions from
/// creation, synced before close — the credential-file shape.
fn write_private_synced(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

// ── provider calls ───────────────────────────────────────────────

/// A provider failure, in the shape the handlers answer with:
/// a status for our own response and a plain-language message.
struct FedError {
    status: u16,
    message: String,
}

impl FedError {
    fn new(status: u16, message: impl Into<String>) -> FedError {
        FedError { status, message: message.into() }
    }
    fn bad_gateway(message: impl Into<String>) -> FedError {
        FedError::new(502, message)
    }
}

type FedResult<T> = Result<T, FedError>;

impl Fed {
    /// GET a JSON document from the provider.
    fn get_json(&self, url: &str) -> FedResult<Value> {
        match self.agent.get(url).call() {
            Ok(resp) => {
                let text = resp.into_string().map_err(|e| FedError::bad_gateway(format!("edit.phext.io sent an unreadable answer: {e}")))?;
                serde_json::from_str(&text).map_err(|e| FedError::bad_gateway(format!("edit.phext.io sent unparseable JSON: {e}")))
            }
            Err(ureq::Error::Status(code, _)) => {
                Err(FedError::bad_gateway(format!("edit.phext.io answered {code} for {url}")))
            }
            Err(e) => Err(FedError::bad_gateway(format!("cannot reach edit.phext.io: {e}"))),
        }
    }

    /// POST a form, returning (status, parsed JSON body) for both
    /// success and OAuth error answers — the token endpoint's
    /// errors are data the flow acts on (invalid_client clears the
    /// cached registration), not transport failures.
    fn post_form(&self, url: &str, pairs: &[(&str, &str)]) -> FedResult<(u16, Value)> {
        let body = form_pairs(pairs);
        let result = self
            .agent
            .post(url)
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string(&body);
        let (status, text) = match result {
            Ok(resp) => (resp.status(), resp.into_string().unwrap_or_default()),
            Err(ureq::Error::Status(code, resp)) => {
                (code, resp.into_string().unwrap_or_default())
            }
            Err(e) => {
                return Err(FedError::bad_gateway(format!(
                    "cannot reach edit.phext.io: {e}"
                )))
            }
        };
        let value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok((status, value))
    }

    /// The provider's discovery document, cached per issuer for
    /// the life of the process. The endpoints fall back to the
    /// conventional paths when discovery itself is unreachable in
    /// shape but the issuer answers — no: discovery failing is a
    /// provider outage or a wrong issuer, and the flow should say
    /// so rather than guess. Only the resource indicator falls
    /// back (to `{issuer}/mcp`, the provider's own construction).
    fn discover(&self) -> FedResult<Discovery> {
        let issuer = self.cfg.issuer.clone();
        if let Some((cached_issuer, d)) = &*self.discovery.lock().unwrap() {
            if *cached_issuer == issuer {
                return Ok(d.clone());
            }
        }
        let meta = self.get_json(&format!(
            "{issuer}/.well-known/oauth-authorization-server"
        ))?;
        let pick = |key: &str, default: String| {
            meta.get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or(default)
        };
        let resource = self
            .get_json(&format!("{issuer}/.well-known/oauth-protected-resource"))
            .ok()
            .and_then(|doc| {
                doc.get("resource")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| format!("{issuer}/mcp"));
        let discovery = Discovery {
            authorization_endpoint: pick(
                "authorization_endpoint",
                format!("{issuer}/oauth/authorize"),
            ),
            token_endpoint: pick("token_endpoint", format!("{issuer}/oauth/token")),
            registration_endpoint: meta
                .get("registration_endpoint")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            resource,
        };
        *self.discovery.lock().unwrap() = Some((issuer, discovery.clone()));
        Ok(discovery)
    }

    /// The cached DCR client for this (issuer, redirect URI), or a
    /// fresh registration. Registration is a first-sign-in event,
    /// not a deploy step: the deploy never touches the provider.
    fn ensure_registration(
        &self,
        discovery: &Discovery,
        redirect_uri: &str,
    ) -> FedResult<String> {
        {
            let state = self.state.lock().unwrap();
            if let Some(reg) = &state.registration {
                if reg.issuer == self.cfg.issuer && reg.redirect_uri == redirect_uri {
                    return Ok(reg.client_id.clone());
                }
            }
        }
        let endpoint = discovery.registration_endpoint.clone().ok_or_else(|| {
            FedError::bad_gateway(
                "edit.phext.io offers no client registration endpoint; federation cannot proceed",
            )
        })?;
        let body = json!({
            "client_name": "Markdown Wizard (md.phext.io)",
            "client_uri": "https://md.phext.io",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "scope": SCOPES,
            "token_endpoint_auth_method": "none",
        });
        let result = self
            .agent
            .post(&endpoint)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        let (status, text) = match result {
            Ok(resp) => (resp.status(), resp.into_string().unwrap_or_default()),
            Err(ureq::Error::Status(code, resp)) => {
                (code, resp.into_string().unwrap_or_default())
            }
            Err(e) => {
                return Err(FedError::bad_gateway(format!(
                    "cannot reach edit.phext.io to register: {e}"
                )))
            }
        };
        if status != 201 && status != 200 {
            return Err(FedError::bad_gateway(format!(
                "client registration was refused ({status}): {text}"
            )));
        }
        let reply: Value = serde_json::from_str(&text).map_err(|e| {
            FedError::bad_gateway(format!("registration answer unparseable: {e}"))
        })?;
        let client_id = reply
            .get("client_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| FedError::bad_gateway("registration answer holds no client_id"))?
            .to_string();
        let mut state = self.state.lock().unwrap();
        state.registration = Some(Registration {
            issuer: self.cfg.issuer.clone(),
            redirect_uri: redirect_uri.to_string(),
            client_id: client_id.clone(),
        });
        self.persist(&state);
        Ok(client_id)
    }

    fn clear_registration(&self) {
        let mut state = self.state.lock().unwrap();
        if state.registration.take().is_some() {
            self.persist(&state);
        }
    }

    /// Exchange an authorization code for the session's first
    /// token pair.
    fn exchange_code(
        &self,
        discovery: &Discovery,
        client_id: &str,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> FedResult<Session> {
        let (status, reply) = self.post_form(
            &discovery.token_endpoint,
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("client_id", client_id),
                ("resource", &discovery.resource),
                ("code_verifier", verifier),
            ],
        )?;
        if status != 200 {
            if reply.get("error").and_then(|v| v.as_str()) == Some("invalid_client") {
                self.clear_registration();
            }
            return Err(FedError::bad_gateway(format!(
                "the token exchange was refused: {}",
                oauth_error_text(&reply)
            )));
        }
        let access = reply
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| FedError::bad_gateway("the token answer holds no access_token"))?;
        let refresh = reply
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| FedError::bad_gateway("the token answer holds no refresh_token"))?;
        let expires_in = reply
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600);
        Ok(Session {
            access_token: access.to_string(),
            refresh_token: refresh.to_string(),
            expires_at: now_unix() + expires_in.saturating_sub(30),
            vault_name: None,
            vault_label: None,
            created_at: now_unix(),
        })
    }

    /// A working access token for the session, refreshed under
    /// the token lock when the stored one has run down. Refresh
    /// rotation means the stored pair is replaced whole; a failed
    /// refresh ends the session (the editor will offer sign-in
    /// again) rather than retrying a dead grant.
    fn access_token(&self, sid: &str) -> FedResult<String> {
        {
            let state = self.state.lock().unwrap();
            match state.sessions.get(sid) {
                Some(s) if s.expires_at > now_unix() => {
                    return Ok(s.access_token.clone());
                }
                Some(_) => {}
                None => {
                    return Err(FedError::new(401, "not signed in"));
                }
            }
        }
        let _guard = self.token_lock.lock().unwrap();
        // Re-check under the lock: another thread may have done it.
        let refresh_token = {
            let state = self.state.lock().unwrap();
            match state.sessions.get(sid) {
                Some(s) if s.expires_at > now_unix() => {
                    return Ok(s.access_token.clone());
                }
                Some(s) => s.refresh_token.clone(),
                None => return Err(FedError::new(401, "not signed in")),
            }
        };
        let discovery = self.discover()?;
        let client_id = {
            let state = self.state.lock().unwrap();
            state
                .registration
                .as_ref()
                .map(|r| r.client_id.clone())
                .ok_or_else(|| FedError::new(401, "sign-in state was lost; sign in again"))?
        };
        let (status, reply) = self.post_form(
            &discovery.token_endpoint,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh_token),
                ("client_id", &client_id),
                ("resource", &discovery.resource),
            ],
        )?;
        if status != 200 {
            let mut state = self.state.lock().unwrap();
            state.sessions.remove(sid);
            self.persist(&state);
            return Err(FedError::new(
                401,
                "your edit.phext.io session has expired — sign in again",
            ));
        }
        let access = reply
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| FedError::bad_gateway("the refresh answer holds no access_token"))?
            .to_string();
        let new_refresh = reply
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or(&refresh_token)
            .to_string();
        let expires_in = reply
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600);
        let mut state = self.state.lock().unwrap();
        if let Some(s) = state.sessions.get_mut(sid) {
            s.access_token = access.clone();
            s.refresh_token = new_refresh;
            s.expires_at = now_unix() + expires_in.saturating_sub(30);
        }
        self.persist(&state);
        Ok(access)
    }

    /// One MCP tool call with the user's own token. The provider's
    /// transport is stateless — `tools/call` is accepted without a
    /// prior `initialize` (its dispatch is a pure function of the
    /// request) — so the client goes straight to the call. A 401
    /// the stored expiry did not predict earns one forced refresh
    /// and one retry; anything else is reported, never retried
    /// blind.
    fn mcp_call(&self, sid: &str, tool: &str, args: Value) -> FedResult<Value> {
        let mut token = self.access_token(sid)?;
        for attempt in 0..2 {
            let body = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": tool, "arguments": args },
            });
            let result = self
                .agent
                .post(&format!("{}/mcp", self.cfg.issuer))
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", "application/json")
                .send_string(&body.to_string());
            let (status, text) = match result {
                Ok(resp) => (resp.status(), resp.into_string().unwrap_or_default()),
                Err(ureq::Error::Status(code, resp)) => {
                    (code, resp.into_string().unwrap_or_default())
                }
                Err(e) => {
                    return Err(FedError::bad_gateway(format!(
                        "cannot reach edit.phext.io: {e}"
                    )))
                }
            };
            if status == 401 && attempt == 0 {
                // Force the refresh path: expire the stored token
                // and let access_token renew it.
                {
                    let mut state = self.state.lock().unwrap();
                    if let Some(s) = state.sessions.get_mut(sid) {
                        s.expires_at = 0;
                    }
                }
                token = self.access_token(sid)?;
                continue;
            }
            if status != 200 {
                let detail = serde_json::from_str::<Value>(&text)
                    .map(|v| oauth_error_text(&v))
                    .unwrap_or(text);
                return Err(FedError::bad_gateway(format!(
                    "edit.phext.io refused the {tool} call ({status}): {detail}"
                )));
            }
            let reply: Value = serde_json::from_str(&text).map_err(|e| {
                FedError::bad_gateway(format!("the MCP answer was unparseable: {e}"))
            })?;
            if let Some(error) = reply.get("error") {
                return Err(FedError::bad_gateway(format!(
                    "edit.phext.io answered the {tool} call with an error: {error}"
                )));
            }
            let result = reply
                .get("result")
                .ok_or_else(|| FedError::bad_gateway("the MCP answer holds no result"))?;
            let is_error = result
                .get("isError")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let text = result
                .get("content")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|c| c.get("text"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| FedError::bad_gateway("the MCP result holds no text"))?;
            if is_error {
                return Err(FedError::bad_gateway(format!(
                    "edit.phext.io refused the {tool} call: {text}"
                )));
            }
            return serde_json::from_str(text).map_err(|e| {
                FedError::bad_gateway(format!("the {tool} result was unparseable: {e}"))
            });
        }
        Err(FedError::bad_gateway("edit.phext.io kept refusing the access token"))
    }
}

/// An OAuth error body as one plain line.
fn oauth_error_text(body: &Value) -> String {
    let error = body.get("error").and_then(|v| v.as_str()).unwrap_or("error");
    match body.get("error_description").and_then(|v| v.as_str()) {
        Some(desc) => format!("{error} — {desc}"),
        None => error.to_string(),
    }
}

// ── the vault ────────────────────────────────────────────────────

/// A new vault name: the recognisable prefix plus ten base32
/// characters drawn from six random bytes — collision-checked
/// against the hosted list before use, and comfortably inside
/// the provider's phext-name rules ([A-Za-z0-9_-], ≤128).
fn new_vault_name() -> std::io::Result<String> {
    let mut buf = [0u8; 6];
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(&mut buf)?;
    const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let mut bits: u64 = 0;
    let mut nbits = 0;
    let mut id = String::with_capacity(10);
    for b in buf {
        bits = (bits << 8) | b as u64;
        nbits += 8;
        while nbits >= 5 && id.len() < 10 {
            nbits -= 5;
            id.push(ALPHABET[((bits >> nbits) & 31) as usize] as char);
        }
    }
    Ok(format!("{VAULT_PREFIX}{id}"))
}

fn valid_vault_name(name: &str) -> bool {
    name.starts_with(VAULT_PREFIX)
        && name.len() > VAULT_PREFIX.len()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The vault manifest's own address: the root scroll.
const VAULT_MANIFEST_COORD: &str = "1.1.1/1.1.1/1.1.1";

/// Bundle n's manifest scroll: Library 1, Shelf 2 — Shelf 1 of
/// the manifest library holds the vault manifest alone at the
/// root, so no bundle number can ever collide with it.
fn bundle_manifest_coord(n: u64) -> String {
    format!("1.2.{n}/1.1.1/1.1.1")
}

/// File m of bundle n (the content shelf, Library 2; Series
/// matches the bundle's manifest).
fn file_coord(n: u64, m: u64) -> String {
    format!("2.1.{n}/1.1.{m}/1.1.1")
}

#[derive(Clone)]
struct VaultBundleRef {
    slug: String,
    n: u64,
    rev: u64,
    updated: u64,
}

#[derive(Clone)]
struct VaultManifest {
    name: String,
    label: String,
    created: u64,
    bundles: Vec<VaultBundleRef>,
}

impl VaultManifest {
    fn render(&self) -> String {
        let mut out = format!(
            "{VAULT_FORMAT}\nvault: {}\nlabel: {}\ncreated: {}\n",
            self.name, self.label, self.created
        );
        for b in &self.bundles {
            out.push_str(&format!(
                "bundle: {} n={} rev={} updated={}\n",
                b.slug, b.n, b.rev, b.updated
            ));
        }
        out
    }

    fn parse(name: &str, text: &str) -> Option<VaultManifest> {
        let mut lines = text.lines();
        if lines.next()?.trim() != VAULT_FORMAT {
            return None;
        }
        let mut manifest = VaultManifest {
            name: name.to_string(),
            label: String::new(),
            created: 0,
            bundles: Vec::new(),
        };
        for line in lines {
            let line = line.trim_end();
            if let Some(v) = line.strip_prefix("vault: ") {
                if v.trim() != name {
                    return None; // a manifest that names another vault
                }
            } else if let Some(v) = line.strip_prefix("label: ") {
                manifest.label = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("created: ") {
                manifest.created = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("bundle: ") {
                let mut parts = v.split_whitespace();
                let slug = parts.next()?.to_string();
                let mut bref = VaultBundleRef { slug, n: 0, rev: 0, updated: 0 };
                for part in parts {
                    if let Some((k, val)) = part.split_once('=') {
                        match k {
                            "n" => bref.n = val.parse().unwrap_or(0),
                            "rev" => bref.rev = val.parse().unwrap_or(0),
                            "updated" => bref.updated = val.parse().unwrap_or(0),
                            _ => {}
                        }
                    }
                }
                if bref.n > 0 {
                    manifest.bundles.push(bref);
                }
            }
        }
        Some(manifest)
    }

    fn bundle(&self, slug: &str) -> Option<&VaultBundleRef> {
        self.bundles.iter().find(|b| b.slug == slug)
    }

    fn next_n(&self) -> u64 {
        self.bundles.iter().map(|b| b.n).max().unwrap_or(0) + 1
    }
}

#[derive(Clone)]
struct VaultFile {
    path: String,
    m: u64,
    rev: u64,
    sha256: String,
}

#[derive(Clone)]
struct BundleManifest {
    slug: String,
    rev: u64,
    updated: u64,
    files: Vec<VaultFile>,
}

impl BundleManifest {
    fn new(slug: &str) -> BundleManifest {
        BundleManifest {
            slug: slug.to_string(),
            rev: 0,
            updated: 0,
            files: Vec::new(),
        }
    }

    fn render(&self) -> String {
        let mut out = format!(
            "{BUNDLE_FORMAT}\nslug: {}\nrev: {}\nupdated: {}\n",
            self.slug, self.rev, self.updated
        );
        let mut files: Vec<&VaultFile> = self.files.iter().collect();
        files.sort_by_key(|f| f.m);
        for f in files {
            out.push_str(&format!(
                "file: {} rev={} sha256={} {}\n",
                f.m, f.rev, f.sha256, f.path
            ));
        }
        out
    }

    fn parse(text: &str) -> Option<BundleManifest> {
        let mut lines = text.lines();
        if lines.next()?.trim() != BUNDLE_FORMAT {
            return None;
        }
        let mut manifest = BundleManifest {
            slug: String::new(),
            rev: 0,
            updated: 0,
            files: Vec::new(),
        };
        for line in lines {
            let line = line.trim_end();
            if let Some(v) = line.strip_prefix("slug: ") {
                manifest.slug = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("rev: ") {
                manifest.rev = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("updated: ") {
                manifest.updated = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("file: ") {
                // `file: <m> rev=<r> sha256=<h> <path>` — the path
                // is the rest of the line (paths may hold spaces,
                // never control characters; the bundle rules see
                // to that), the same trick as the store's stamps.
                let mut parts = v.splitn(4, ' ');
                let m: u64 = parts.next()?.parse().ok()?;
                let mut vf = VaultFile {
                    path: String::new(),
                    m,
                    rev: 0,
                    sha256: String::new(),
                };
                if let Some(rev) = parts.next().and_then(|p| p.strip_prefix("rev=")) {
                    vf.rev = rev.parse().unwrap_or(0);
                }
                if let Some(hash) = parts.next().and_then(|p| p.strip_prefix("sha256=")) {
                    vf.sha256 = hash.to_string();
                }
                vf.path = parts.next()?.to_string();
                if !vf.path.is_empty() {
                    manifest.files.push(vf);
                }
            }
        }
        if manifest.slug.is_empty() {
            return None;
        }
        Some(manifest)
    }

    fn file(&self, path: &str) -> Option<&VaultFile> {
        self.files.iter().find(|f| f.path == path)
    }

    fn next_m(&self) -> u64 {
        self.files.iter().map(|f| f.m).max().unwrap_or(0) + 1
    }
}

impl Fed {
    /// The hosted phext names (suffix-normalized), from the
    /// user's own token.
    fn list_hosted(&self, sid: &str) -> FedResult<Vec<String>> {
        let reply = self.mcp_call(sid, "list_phexts", json!({}))?;
        let mut names = Vec::new();
        if let Some(list) = reply.get("phexts").and_then(|v| v.as_array()) {
            for entry in list {
                if let Some(name) = entry.get("name").and_then(|v| v.as_str()) {
                    names.push(
                        name.strip_suffix(".phext").unwrap_or(name).to_string(),
                    );
                }
            }
        }
        Ok(names)
    }

    fn read_scroll(
        &self,
        sid: &str,
        phext: &str,
        coord: &str,
    ) -> FedResult<Option<String>> {
        let reply = self.mcp_call(
            sid,
            "read_scroll",
            json!({ "phext": phext, "coordinate": coord }),
        )?;
        let exists = reply
            .get("exists")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !exists {
            return Ok(None);
        }
        Ok(Some(
            reply
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ))
    }

    fn write_scroll(
        &self,
        sid: &str,
        phext: &str,
        coord: &str,
        content: &str,
    ) -> FedResult<()> {
        self.mcp_call(
            sid,
            "write_scroll",
            json!({ "phext": phext, "coordinate": coord, "content": content }),
        )?;
        Ok(())
    }

    fn read_vault_manifest(
        &self,
        sid: &str,
        name: &str,
    ) -> FedResult<Option<VaultManifest>> {
        match self.read_scroll(sid, name, VAULT_MANIFEST_COORD)? {
            Some(text) => Ok(VaultManifest::parse(name, &text)),
            None => Ok(None),
        }
    }

    fn read_bundle_manifest(
        &self,
        sid: &str,
        name: &str,
        n: u64,
    ) -> FedResult<Option<BundleManifest>> {
        match self.read_scroll(sid, name, &bundle_manifest_coord(n))? {
            Some(text) => Ok(BundleManifest::parse(&text)),
            None => Ok(None),
        }
    }

    /// Every hosted `mdwiz-vault-*` phext with a readable
    /// manifest of ours: the discovery a fresh browser offers its
    /// user ("found these vaults — pick yours, or start one").
    fn discover_vaults(&self, sid: &str) -> FedResult<Vec<VaultManifest>> {
        let hosted = self.list_hosted(sid)?;
        let mut found = Vec::new();
        for name in hosted.iter().filter(|n| valid_vault_name(n)).take(20) {
            if let Some(manifest) = self.read_vault_manifest(sid, name)? {
                found.push(manifest);
            }
        }
        Ok(found)
    }
}

// ── sync planning ────────────────────────────────────────────────

/// One file as the browser reports it for a sync.
#[derive(Clone)]
struct LocalFile {
    path: String,
    hash: String,
    /// The text, present only when the browser believes the file
    /// changed since the base (or the server asks again via a
    /// conflict resolution).
    text: Option<String>,
    base_rev: u64,
    base_hash: String,
}

/// What a push should do with one file — the whole conflict
/// policy in one function, so the editor's mirror of it and the
/// tests can both be checked against the same table.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Plan {
    /// Contents already agree; only the base moves.
    Skip,
    /// Local changed, vault stood still: write.
    Write,
    /// Vault changed, local stood still: leave it, report it.
    VaultNewer,
    /// Both changed (or two births): refuse; the user decides.
    Conflict,
}

fn plan_push(local: &LocalFile, vault: Option<&VaultFile>, force: bool) -> Plan {
    match vault {
        None => {
            if local.base_rev == 0 {
                Plan::Write // first time the vault sees this file
            } else if local.hash == local.base_hash {
                Plan::Skip // it was in the vault, the vault lost it, local stood still
            } else if force {
                Plan::Write
            } else {
                Plan::Conflict // the vault lost a file the local side kept editing
            }
        }
        Some(vf) => {
            if local.hash == vf.sha256 {
                return Plan::Skip;
            }
            let local_changed = local.hash != local.base_hash;
            let vault_changed = vf.rev != local.base_rev || vf.sha256 != local.base_hash;
            match (local_changed, vault_changed) {
                (true, false) => Plan::Write,
                (false, true) => Plan::VaultNewer,
                (false, false) => Plan::Skip, // hashes differ from base alike: unreachable, safe
                (true, true) => {
                    if force {
                        Plan::Write
                    } else {
                        Plan::Conflict
                    }
                }
            }
        }
    }
}

/// The same table from the pull side: what to do with one vault
/// file, given the local state the browser reported.
#[derive(Clone, Copy, PartialEq, Debug)]
enum PullPlan {
    /// Take the vault's text.
    Send,
    /// Local is ahead or contents agree: leave local alone.
    Keep,
    Conflict,
}

fn plan_pull(
    local: Option<&LocalFile>,
    vf: &VaultFile,
    take: bool,
) -> PullPlan {
    match local {
        None => PullPlan::Send, // the vault has a file this browser lacks
        Some(l) => {
            if l.hash == vf.sha256 {
                return PullPlan::Keep;
            }
            let local_changed = l.hash != l.base_hash;
            let vault_changed = vf.rev != l.base_rev || vf.sha256 != l.base_hash;
            match (local_changed, vault_changed) {
                (false, true) => PullPlan::Send,
                (true, false) => PullPlan::Keep,
                (false, false) => PullPlan::Keep,
                (true, true) => {
                    if take {
                        PullPlan::Send
                    } else {
                        PullPlan::Conflict
                    }
                }
            }
        }
    }
}

// ── handlers ─────────────────────────────────────────────────────

fn send_json(
    stream: &mut TcpStream,
    headers: &Map<String, String>,
    status: u16,
    value: &Value,
) {
    http::send_response(
        stream,
        status,
        JSON,
        value.to_string().as_bytes(),
        CacheClass::NoStore,
        headers,
        false,
    );
}

fn send_json_error(
    stream: &mut TcpStream,
    headers: &Map<String, String>,
    status: u16,
    message: &str,
) {
    send_json(
        stream,
        headers,
        status,
        &json!({ "ok": false, "error": message }),
    );
}

fn redirect_to(
    stream: &mut TcpStream,
    headers: &Map<String, String>,
    location: &str,
    extra: &str,
) {
    // The Location value goes out byte-exact: it is built from
    // form-encoded parts, and escaping it (as HTML text would
    // want) would mangle every query parameter after the first.
    let extra = format!("Location: {location}\r\n{extra}");
    http::send_response_extra(
        stream,
        302,
        TEXT,
        format!("moved: {location}\n").as_bytes(),
        CacheClass::NoStore,
        headers,
        false,
        &extra,
    );
}

/// A small standalone page in the site's register, for the two
/// moments the flow must speak to the browser directly (a refused
/// sign-in, a dead link). The published pages' theme snippet rides
/// along, so the page honours the same stored choice.
fn message_page(
    stream: &mut TcpStream,
    headers: &Map<String, String>,
    status: u16,
    title: &str,
    body: &str,
) {
    let page = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title} — md.phext.io</title><link rel=\"icon\" href=\"/favicon.svg\">\
         <script>{}</script>\
         <style>body{{font-family:Georgia,serif;background:#f7f6f1;color:#20211c;\
         margin:0;padding:4rem 1.25rem}}main{{max-width:36rem;margin:0 auto}}\
         h1{{font-weight:600}}a{{color:#3c3f94}}p{{line-height:1.6}}</style></head>\
         <body><main><h1>{title}</h1><p>{body}</p>\
         <p><a href=\"/\">Back to writing</a></p></main></body></html>",
        crate::render::THEME_SNIPPET,
        title = http::html_escape(title),
        body = body,
    );
    http::send_response(stream, status, HTML, page.as_bytes(), CacheClass::NoStore, headers, false);
}

impl Fed {
    /// GET /auth/login — begin the flow. The browser is sent to
    /// edit.phext.io's authorization endpoint with a PKCE
    /// challenge; the verifier stays in this server's memory,
    /// keyed by the state nonce, for ten minutes.
    pub fn handle_login(
        &self,
        stream: &mut TcpStream,
        headers: &Map<String, String>,
        public_base: &str,
    ) {
        if !self.cfg.enabled {
            return send_json_error(stream, headers, 404, "federation is off on this deployment");
        }
        let redirect_uri = format!("{public_base}/auth/callback");
        let begin = (|| -> FedResult<String> {
            let discovery = self.discover()?;
            let client_id = self.ensure_registration(&discovery, &redirect_uri)?;
            let state = random_hex(16).map_err(|e| FedError::new(500, e.to_string()))?;
            let verifier = random_hex(32).map_err(|e| FedError::new(500, e.to_string()))?;
            {
                let mut pending = self.pending.lock().unwrap();
                let now = now_unix();
                pending.retain(|_, f| now - f.created_at < FLOW_TTL);
                pending.insert(
                    state.clone(),
                    PendingFlow {
                        verifier: verifier.clone(),
                        redirect_uri: redirect_uri.clone(),
                        created_at: now,
                    },
                );
            }
            let query = form_pairs(&[
                ("response_type", "code"),
                ("client_id", &client_id),
                ("redirect_uri", &redirect_uri),
                ("scope", SCOPES),
                ("state", &state),
                ("code_challenge", &pkce_challenge(&verifier)),
                ("code_challenge_method", "S256"),
                ("resource", &discovery.resource),
            ]);
            Ok(format!("{}?{query}", discovery.authorization_endpoint))
        })();
        match begin {
            Ok(location) => redirect_to(stream, headers, &location, ""),
            Err(e) => message_page(
                stream,
                headers,
                e.status,
                "Sign-in could not begin",
                &format!(
                    "{} You can keep writing — drafts on this device are unaffected.",
                    http::html_escape(&e.message)
                ),
            ),
        }
    }

    /// GET /auth/callback — the provider returns the browser
    /// here with a code (or an error). The exchange happens
    /// server-to-server; the browser leaves with only our own
    /// session cookie.
    pub fn handle_callback(
        &self,
        stream: &mut TcpStream,
        headers: &Map<String, String>,
        target: &str,
    ) {
        if !self.cfg.enabled {
            return send_json_error(stream, headers, 404, "federation is off on this deployment");
        }
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_pairs(query);
        if let Some(error) = params.get("error") {
            // An error answer clears the cached registration only
            // when it belongs to a flow this server actually
            // started (its state nonce is pending, and is consumed
            // here): the callback is an unauthenticated address,
            // and a bare ?error=invalid_client from anywhere must
            // not be able to wipe the client registration.
            let flow_is_ours = params
                .get("state")
                .map(|s| self.pending.lock().unwrap().remove(s).is_some())
                .unwrap_or(false);
            if error == "invalid_client" && flow_is_ours {
                self.clear_registration();
            }
            let desc = params
                .get("error_description")
                .cloned()
                .unwrap_or_else(|| error.clone());
            return message_page(
                stream,
                headers,
                200,
                "Sign-in was not completed",
                &format!(
                    "edit.phext.io answered: {}. Nothing was changed; your drafts here are as they were.",
                    http::html_escape(&desc)
                ),
            );
        }
        let (Some(code), Some(state)) = (params.get("code"), params.get("state")) else {
            return message_page(
                stream,
                headers,
                400,
                "That sign-in link is incomplete",
                "It carries no authorization code. Start again from the editor's sign-in button.",
            );
        };
        let flow = {
            let mut pending = self.pending.lock().unwrap();
            pending.remove(state)
        };
        let Some(flow) = flow.filter(|f| now_unix() - f.created_at < FLOW_TTL) else {
            return message_page(
                stream,
                headers,
                400,
                "That sign-in link has expired",
                "Authorization links live for ten minutes and work once. Start again from the editor's sign-in button.",
            );
        };
        let finish = (|| -> FedResult<String> {
            let discovery = self.discover()?;
            let client_id = {
                let state = self.state.lock().unwrap();
                state
                    .registration
                    .as_ref()
                    .filter(|r| r.issuer == self.cfg.issuer && r.redirect_uri == flow.redirect_uri)
                    .map(|r| r.client_id.clone())
                    .ok_or_else(|| FedError::new(400, "the client registration was lost mid-flow; sign in again"))?
            };
            let session = self.exchange_code(
                &discovery,
                &client_id,
                code,
                &flow.redirect_uri,
                &flow.verifier,
            )?;
            let sid = random_hex(16).map_err(|e| FedError::new(500, e.to_string()))?;
            let mut state = self.state.lock().unwrap();
            state.sessions.insert(sid.clone(), session);
            self.persist(&state);
            Ok(sid)
        })();
        match finish {
            Ok(sid) => {
                let secure = if http::forwarded_https(headers) { "; Secure" } else { "" };
                let cookie = format!(
                    "Set-Cookie: {SESSION_COOKIE}={sid}; HttpOnly; Path=/; SameSite=Lax; Max-Age={SESSION_MAX_AGE}{secure}\r\n"
                );
                redirect_to(stream, headers, "/?signed-in=1", &cookie);
            }
            Err(e) => message_page(
                stream,
                headers,
                e.status,
                "Sign-in could not be completed",
                &format!(
                    "{} Your drafts here are as they were.",
                    http::html_escape(&e.message)
                ),
            ),
        }
    }

    /// POST /auth/logout — forget the session here. (The provider
    /// exposes no revocation endpoint; the grant itself expires
    /// on its own schedule. Our tokens are gone from this server
    /// the moment this answers.)
    pub fn handle_logout(&self, stream: &mut TcpStream, headers: &Map<String, String>) {
        if let Some(sid) = self.session_id(headers) {
            let mut state = self.state.lock().unwrap();
            if state.sessions.remove(&sid).is_some() {
                self.persist(&state);
            }
        }
        let secure = if http::forwarded_https(headers) { "; Secure" } else { "" };
        let cookie = format!(
            "Set-Cookie: {SESSION_COOKIE}=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0{secure}\r\n"
        );
        let body = json!({ "ok": true }).to_string();
        http::send_response_extra(
            stream,
            200,
            JSON,
            body.as_bytes(),
            CacheClass::NoStore,
            headers,
            false,
            &cookie,
        );
    }

    /// GET /api/me — the editor's one boot question. Deliberately
    /// makes no provider calls: it reports configuration and the
    /// session this server already holds.
    pub fn handle_me(
        &self,
        stream: &mut TcpStream,
        headers: &Map<String, String>,
        head_only: bool,
    ) {
        let session = if self.cfg.enabled {
            self.session(headers).map(|(_, s)| s)
        } else {
            None
        };
        let body = json!({
            "ok": true,
            "federation": self.cfg.enabled,
            "issuer": self.cfg.issuer,
            "signed_in": session.is_some(),
            "vault_name": session.as_ref().and_then(|s| s.vault_name.clone()),
            "vault_label": session.as_ref().and_then(|s| s.vault_label.clone()),
        });
        http::send_response(
            stream,
            200,
            JSON,
            body.to_string().as_bytes(),
            CacheClass::NoStore,
            headers,
            head_only,
        );
    }

    /// The session a vault handler needs, or the clean refusal
    /// the editor is built to hear.
    fn require_session(
        &self,
        stream: &mut TcpStream,
        headers: &Map<String, String>,
    ) -> Option<(String, Session)> {
        if !self.cfg.enabled {
            send_json_error(stream, headers, 404, "federation is off on this deployment");
            return None;
        }
        match self.session(headers) {
            Some(pair) => Some(pair),
            None => {
                send_json_error(stream, headers, 401, "not signed in");
                None
            }
        }
    }

    /// Where the chosen vault stands: hosted or not, its
    /// manifest if it has one.
    fn vault_context(
        &self,
        sid: &str,
        session: &Session,
    ) -> FedResult<Option<(String, bool, Option<VaultManifest>)>> {
        let Some(name) = session.vault_name.clone() else {
            return Ok(None);
        };
        let hosted = self.list_hosted(sid)?.iter().any(|n| n == &name);
        let manifest = if hosted {
            self.read_vault_manifest(sid, &name)?
        } else {
            None
        };
        Ok(Some((name, hosted, manifest)))
    }

    /// GET /api/vault — the vault dialog's whole picture.
    pub fn handle_vault_status(
        &self,
        stream: &mut TcpStream,
        headers: &Map<String, String>,
        head_only: bool,
    ) {
        let Some((sid, session)) = self.require_session(stream, headers) else {
            return;
        };
        let mut body = json!({
            "ok": true,
            "signed_in": true,
            "vault": Value::Null,
            "found": [],
            "bundles": [],
        });
        match self.vault_context(&sid, &session) {
            Ok(Some((name, hosted, manifest))) => {
                body["vault"] = json!({
                    "name": name,
                    "label": session.vault_label.clone().unwrap_or_default(),
                    "hosted": hosted,
                    "starter": "/api/vault/starter",
                });
                if let Some(manifest) = &manifest {
                    body["bundles"] = Value::Array(
                        manifest
                            .bundles
                            .iter()
                            .map(|b| {
                                json!({
                                    "slug": b.slug,
                                    "rev": b.rev,
                                    "updated": b.updated,
                                })
                            })
                            .collect(),
                    );
                }
            }
            Ok(None) => {}
            Err(e) => {
                body["provider_error"] = Value::String(e.message);
            }
        }
        match self.discover_vaults(&sid) {
            Ok(found) => {
                body["found"] = Value::Array(
                    found
                        .iter()
                        .map(|m| {
                            json!({
                                "name": m.name,
                                "label": m.label,
                                "bundles": m.bundles.iter().map(|b| b.slug.clone()).collect::<Vec<_>>(),
                            })
                        })
                        .collect(),
                );
            }
            Err(e) => {
                if body.get("provider_error").is_none() {
                    body["provider_error"] = Value::String(e.message);
                }
            }
        }
        http::send_response(
            stream,
            200,
            JSON,
            body.to_string().as_bytes(),
            CacheClass::NoStore,
            headers,
            head_only,
        );
    }

    /// POST /api/vault/new {label} — choose a fresh vault name
    /// for this session. The phext itself does not exist until it
    /// is hosted; the starter download carries its manifest.
    pub fn handle_vault_new(
        &self,
        stream: &mut TcpStream,
        reader: &mut BufReader<TcpStream>,
        headers: &Map<String, String>,
    ) {
        let Some((sid, _session)) = self.require_session(stream, headers) else {
            return;
        };
        let body = read_json_body(stream, reader, headers);
        let Some(body) = body else { return };
        let label = clean_label(body.get("label").and_then(|v| v.as_str()).unwrap_or(""));
        let made = (|| -> FedResult<(String, bool)> {
            let hosted = self.list_hosted(&sid)?;
            for _ in 0..5 {
                let name = new_vault_name().map_err(|e| FedError::new(500, e.to_string()))?;
                if !hosted.iter().any(|n| n == &name) {
                    return Ok((name, false));
                }
            }
            Err(FedError::new(500, "could not find a free vault name"))
        })();
        match made {
            Ok((name, hosted)) => {
                let mut state = self.state.lock().unwrap();
                if let Some(s) = state.sessions.get_mut(&sid) {
                    s.vault_name = Some(name.clone());
                    s.vault_label = Some(label.clone());
                }
                self.persist(&state);
                send_json(
                    stream,
                    headers,
                    200,
                    &json!({
                        "ok": true,
                        "vault": { "name": name, "label": label, "hosted": hosted, "starter": "/api/vault/starter" },
                    }),
                );
            }
            Err(e) => send_json_error(stream, headers, e.status, &e.message),
        }
    }

    /// POST /api/vault/choose {name, label?} — adopt a hosted
    /// vault as this session's. A root scroll that is present but
    /// not ours refuses; an absent one means an empty phext the
    /// first sync will initialize.
    pub fn handle_vault_choose(
        &self,
        stream: &mut TcpStream,
        reader: &mut BufReader<TcpStream>,
        headers: &Map<String, String>,
    ) {
        let Some((sid, _session)) = self.require_session(stream, headers) else {
            return;
        };
        let Some(body) = read_json_body(stream, reader, headers) else {
            return;
        };
        let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if !valid_vault_name(&name) {
            return send_json_error(stream, headers, 400, "that is not a markdown-wizard vault name");
        }
        let chosen = (|| -> FedResult<String> {
            let hosted = self.list_hosted(&sid)?;
            if !hosted.iter().any(|n| n == &name) {
                return Err(FedError::new(
                    404,
                    "that phext is not hosted on edit.phext.io yet",
                ));
            }
            match self.read_scroll(&sid, &name, VAULT_MANIFEST_COORD)? {
                Some(text) => match VaultManifest::parse(&name, &text) {
                    Some(m) => Ok(m.label),
                    None => Err(FedError::new(
                        422,
                        "that phext's root scroll is not a markdown-wizard vault manifest",
                    )),
                },
                None => Ok(body
                    .get("label")
                    .and_then(|v| v.as_str())
                    .map(clean_label)
                    .unwrap_or_else(|| "My Markdown Wizard vault".to_string())),
            }
        })();
        match chosen {
            Ok(label) => {
                let mut state = self.state.lock().unwrap();
                if let Some(s) = state.sessions.get_mut(&sid) {
                    s.vault_name = Some(name.clone());
                    s.vault_label = Some(label.clone());
                }
                self.persist(&state);
                send_json(
                    stream,
                    headers,
                    200,
                    &json!({
                        "ok": true,
                        "vault": { "name": name, "label": label, "hosted": true, "starter": "/api/vault/starter" },
                    }),
                );
            }
            Err(e) => send_json_error(stream, headers, e.status, &e.message),
        }
    }

    /// GET /api/vault/starter — the vault as a one-scroll phext
    /// file: the manifest itself, at the root coordinate, which
    /// is all a phext with one scroll at 1.1.1/1.1.1/1.1.1 is.
    /// The user hosts this file (edit.phext.io/submit exists for
    /// exactly this) and the vault is live.
    pub fn handle_vault_starter(
        &self,
        stream: &mut TcpStream,
        headers: &Map<String, String>,
        head_only: bool,
    ) {
        let Some((_sid, session)) = self.require_session(stream, headers) else {
            return;
        };
        let Some(name) = session.vault_name.clone() else {
            return send_json_error(stream, headers, 409, "choose or start a vault first");
        };
        let manifest = VaultManifest {
            name: name.clone(),
            label: session
                .vault_label
                .clone()
                .unwrap_or_else(|| "My Markdown Wizard vault".to_string()),
            created: now_unix(),
            bundles: Vec::new(),
        };
        let extra = format!(
            "Content-Disposition: attachment; filename=\"{name}.phext\"\r\n"
        );
        http::send_response_extra(
            stream,
            200,
            TEXT,
            manifest.render().as_bytes(),
            CacheClass::NoStore,
            headers,
            head_only,
            &extra,
        );
    }
}

/// Read and parse a JSON request body for the vault endpoints.
fn read_json_body(
    stream: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    headers: &Map<String, String>,
) -> Option<Value> {
    let bytes = match http::read_body(reader, headers, 40 * 1024 * 1024) {
        Some(b) => b,
        None => {
            send_json_error(stream, headers, 413, "request body larger than the limit");
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(_) => {
            send_json_error(stream, headers, 400, "request body is not JSON");
            None
        }
    }
}

/// Parse the browser's file list for a sync or pull. Entries are
/// validated against the bundle's own path rules — a path the
/// bundle could not hold is refused whole-request, like a bad
/// publish.
fn parse_local_files(body: &Value, key: &str) -> Result<Vec<LocalFile>, String> {
    let Some(items) = body.get(key).and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    if items.len() > MAX_FILES {
        return Err(format!("more than {MAX_FILES} files in one sync"));
    }
    let mut out = Vec::new();
    for item in items {
        let path = item.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if !valid_bundle_path(path) {
            return Err(format!("unsafe file path in sync: {path:?}"));
        }
        let text = item.get("text").and_then(|v| v.as_str()).map(|s| s.to_string());
        if let Some(t) = &text {
            if t.len() > MAX_FILE_BYTES as usize {
                return Err(format!("file {path} is larger than the sync limit"));
            }
        }
        out.push(LocalFile {
            path: path.to_string(),
            hash: item
                .get("hash")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            text,
            base_rev: item.get("base_rev").and_then(|v| v.as_u64()).unwrap_or(0),
            base_hash: item
                .get("base_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        });
    }
    Ok(out)
}

fn parse_deleted(body: &Value) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    if let Some(items) = body.get("deleted").and_then(|v| v.as_array()) {
        for item in items {
            if let Some(path) = item.get("path").and_then(|v| v.as_str()) {
                if valid_bundle_path(path) {
                    out.push((
                        path.to_string(),
                        item.get("base_rev").and_then(|v| v.as_u64()).unwrap_or(0),
                    ));
                }
            }
        }
    }
    out
}

fn parse_path_list(body: &Value, key: &str) -> Vec<String> {
    body.get(key)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

impl Fed {
    /// The vault context both sync directions need, answering
    /// the refusals itself. Ok(None) means "already answered".
    fn sync_context(
        &self,
        stream: &mut TcpStream,
        headers: &Map<String, String>,
        sid: &str,
        session: &Session,
    ) -> Result<Option<(String, Option<VaultManifest>)>, ()> {
        match self.vault_context(sid, session) {
            Ok(Some((name, true, manifest))) => Ok(Some((name, manifest))),
            Ok(Some((name, false, _))) => {
                send_json(
                    stream,
                    headers,
                    409,
                    &json!({
                        "ok": false,
                        "error": "vault_not_hosted",
                        "message": format!("{name} is not hosted on edit.phext.io yet"),
                        "starter": "/api/vault/starter",
                    }),
                );
                Err(())
            }
            Ok(None) => {
                send_json_error(stream, headers, 409, "choose or start a vault first");
                Err(())
            }
            Err(e) => {
                send_json_error(stream, headers, e.status, &e.message);
                Err(())
            }
        }
    }

    /// POST /api/vault/sync — push one bundle's local state
    /// toward the vault. See the module header for the model;
    /// the planning table is `plan_push`, and this function is
    /// its executor: file scrolls first, then the bundle
    /// manifest, then the vault manifest, so an interruption
    /// leaves at worst unlisted content a later sync re-plans.
    pub fn handle_vault_sync(
        &self,
        stream: &mut TcpStream,
        reader: &mut BufReader<TcpStream>,
        headers: &Map<String, String>,
    ) {
        let Some((sid, session)) = self.require_session(stream, headers) else {
            return;
        };
        let Some(body) = read_json_body(stream, reader, headers) else {
            return;
        };
        let slug = body.get("bundle").and_then(|v| v.as_str()).unwrap_or("");
        if !crate::bundle::valid_slug(slug) {
            return send_json_error(stream, headers, 400, "the bundle name is not a valid slug");
        }
        let files = match parse_local_files(&body, "files") {
            Ok(f) => f,
            Err(e) => return send_json_error(stream, headers, 400, &e),
        };
        let deleted = parse_deleted(&body);
        let force: Vec<String> = parse_path_list(&body, "force");
        let _guard = self.vault_lock.lock().unwrap();
        let Ok(Some((name, manifest))) = self.sync_context(stream, headers, &sid, &session)
        else {
            return;
        };
        let run = (|| -> FedResult<Value> {
            let mut vault = manifest.unwrap_or_else(|| VaultManifest {
                name: name.clone(),
                label: session
                    .vault_label
                    .clone()
                    .unwrap_or_else(|| "My Markdown Wizard vault".to_string()),
                created: now_unix(),
                bundles: Vec::new(),
            });
            let existing = vault.bundle(slug).map(|b| (b.n, b.rev));
            let (n, _) = match existing {
                Some(pair) => pair,
                None => {
                    if vault.bundles.len() >= MAX_VAULT_BUNDLES {
                        return Err(FedError::new(
                            422,
                            "this vault already holds the maximum number of bundles",
                        ));
                    }
                    (vault.next_n(), 0)
                }
            };
            let mut bm = self
                .read_bundle_manifest(&sid, &name, n)?
                .unwrap_or_else(|| BundleManifest::new(slug));
            let mut changed = existing.is_none();
            let mut written: Vec<String> = Vec::new();
            let mut deleted_done: Vec<String> = Vec::new();
            let mut vault_newer: Vec<String> = Vec::new();
            let mut conflicts: Vec<Value> = Vec::new();
            let mut not_carried: Vec<String> = Vec::new();
            let mut base_updates: Vec<Value> = Vec::new();
            for local in &files {
                let vf = bm.file(&local.path).cloned();
                match plan_push(local, vf.as_ref(), force.iter().any(|p| p == &local.path)) {
                    Plan::Skip => {
                        if let Some(vf) = &vf {
                            base_updates.push(json!({
                                "path": local.path, "rev": vf.rev, "hash": vf.sha256,
                            }));
                        } else {
                            base_updates.push(json!({
                                "path": local.path, "rev": 0, "hash": "",
                            }));
                        }
                    }
                    Plan::Write => {
                        let Some(text) = &local.text else {
                            not_carried.push(local.path.clone());
                            continue;
                        };
                        let hash = sha256_hex(text.as_bytes());
                        let (m, rev) = match &vf {
                            Some(vf) => (vf.m, vf.rev + 1),
                            None => (bm.next_m(), 1),
                        };
                        self.write_scroll(&sid, &name, &file_coord(n, m), text)?;
                        let entry = VaultFile {
                            path: local.path.clone(),
                            m,
                            rev,
                            sha256: hash.clone(),
                        };
                        if let Some(slot) = bm.files.iter_mut().find(|f| f.path == local.path) {
                            *slot = entry;
                        } else {
                            bm.files.push(entry);
                        }
                        changed = true;
                        written.push(local.path.clone());
                        base_updates.push(json!({
                            "path": local.path, "rev": rev, "hash": hash,
                        }));
                    }
                    Plan::VaultNewer => vault_newer.push(local.path.clone()),
                    Plan::Conflict => conflicts.push(json!({
                        "path": local.path,
                        "vault_rev": vf.as_ref().map(|v| v.rev).unwrap_or(0),
                        "vault_hash": vf.as_ref().map(|v| v.sha256.clone()).unwrap_or_default(),
                    })),
                }
            }
            for (path, base_rev) in &deleted {
                match bm.file(path).cloned() {
                    None => {
                        base_updates.push(json!({ "path": path, "rev": 0, "hash": "" }));
                    }
                    Some(vf) if vf.rev == *base_rev => {
                        self.write_scroll(&sid, &name, &file_coord(n, vf.m), "")?;
                        bm.files.retain(|f| f.path != *path);
                        changed = true;
                        deleted_done.push(path.clone());
                        base_updates.push(json!({ "path": path, "rev": 0, "hash": "" }));
                    }
                    Some(vf) => conflicts.push(json!({
                        "path": path,
                        "vault_rev": vf.rev,
                        "vault_hash": vf.sha256,
                    })),
                }
            }
            if changed {
                bm.rev += 1;
                bm.updated = now_unix();
                self.write_scroll(&sid, &name, &bundle_manifest_coord(n), &bm.render())?;
                match vault.bundles.iter_mut().find(|b| b.slug == slug) {
                    Some(slot) => {
                        slot.rev = bm.rev;
                        slot.updated = bm.updated;
                    }
                    None => vault.bundles.push(VaultBundleRef {
                        slug: slug.to_string(),
                        n,
                        rev: bm.rev,
                        updated: bm.updated,
                    }),
                }
                self.write_scroll(&sid, &name, VAULT_MANIFEST_COORD, &vault.render())?;
            }
            Ok(json!({
                "ok": true,
                "bundle": slug,
                "bundle_rev": bm.rev,
                "files": base_updates,
                "written": written,
                "deleted": deleted_done,
                "vault_newer": vault_newer,
                "conflicts": conflicts,
                "not_carried": not_carried,
            }))
        })();
        match run {
            Ok(value) => send_json(stream, headers, 200, &value),
            Err(e) => send_json_error(stream, headers, e.status, &e.message),
        }
    }

    /// POST /api/vault/pull — bring the vault's copy of one
    /// bundle toward this browser. Nothing here writes anywhere:
    /// the answer carries the texts to take, the conflicts to
    /// decide, and the honest lists (what local is ahead on, what
    /// local deleted and the vault did not resurrect).
    pub fn handle_vault_pull(
        &self,
        stream: &mut TcpStream,
        reader: &mut BufReader<TcpStream>,
        headers: &Map<String, String>,
    ) {
        let Some((sid, session)) = self.require_session(stream, headers) else {
            return;
        };
        let Some(body) = read_json_body(stream, reader, headers) else {
            return;
        };
        let slug = body.get("bundle").and_then(|v| v.as_str()).unwrap_or("");
        if !crate::bundle::valid_slug(slug) {
            return send_json_error(stream, headers, 400, "the bundle name is not a valid slug");
        }
        let local_files = match parse_local_files(&body, "local") {
            Ok(f) => f,
            Err(e) => return send_json_error(stream, headers, 400, &e),
        };
        let deleted = parse_deleted(&body);
        let take: Vec<String> = parse_path_list(&body, "take");
        let _guard = self.vault_lock.lock().unwrap();
        let Ok(Some((name, manifest))) = self.sync_context(stream, headers, &sid, &session)
        else {
            return;
        };
        let empty = json!({
            "ok": true, "exists": false, "bundle": slug,
            "files": [], "conflicts": [], "local_ahead": [], "kept_deleted": [],
        });
        let run = (|| -> FedResult<Value> {
            let Some(manifest) = manifest else {
                return Ok(empty.clone());
            };
            let Some(bref) = manifest.bundle(slug).cloned() else {
                return Ok(empty.clone());
            };
            let Some(bm) = self.read_bundle_manifest(&sid, &name, bref.n)? else {
                return Ok(empty.clone());
            };
            let mut files_out: Vec<Value> = Vec::new();
            let mut conflicts: Vec<Value> = Vec::new();
            let mut local_ahead: Vec<String> = Vec::new();
            let mut kept_deleted: Vec<String> = Vec::new();
            for vf in &bm.files {
                if let Some((_, base_rev)) = deleted.iter().find(|(p, _)| p == &vf.path) {
                    if vf.rev == *base_rev {
                        kept_deleted.push(vf.path.clone());
                    } else {
                        conflicts.push(json!({
                            "path": vf.path,
                            "vault_rev": vf.rev,
                            "vault_hash": vf.sha256,
                        }));
                    }
                    continue;
                }
                let local = local_files.iter().find(|f| f.path == vf.path);
                match plan_pull(local, vf, take.iter().any(|p| p == &vf.path)) {
                    PullPlan::Send => {
                        let text = self
                            .read_scroll(&sid, &name, &file_coord(bref.n, vf.m))?
                            .unwrap_or_default();
                        files_out.push(json!({
                            "path": vf.path,
                            "text": text,
                            "rev": vf.rev,
                            "hash": vf.sha256,
                        }));
                    }
                    PullPlan::Keep => {
                        if let Some(l) = local {
                            let local_changed = l.hash != l.base_hash;
                            let vault_changed =
                                vf.rev != l.base_rev || vf.sha256 != l.base_hash;
                            if local_changed && !vault_changed {
                                local_ahead.push(vf.path.clone());
                            }
                        }
                    }
                    PullPlan::Conflict => conflicts.push(json!({
                        "path": vf.path,
                        "vault_rev": vf.rev,
                        "vault_hash": vf.sha256,
                    })),
                }
            }
            Ok(json!({
                "ok": true,
                "exists": true,
                "bundle": slug,
                "bundle_rev": bm.rev,
                "files": files_out,
                "conflicts": conflicts,
                "local_ahead": local_ahead,
                "kept_deleted": kept_deleted,
            }))
        })();
        match run {
            Ok(value) => send_json(stream, headers, 200, &value),
            Err(e) => send_json_error(stream, headers, e.status, &e.message),
        }
    }
}

// ── tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_the_rfc_7636_vector() {
        // The worked example from RFC 7636 appendix B — the one
        // vector every PKCE implementation must agree on.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn form_codec_round_trips_callback_shapes() {
        let pairs = form_pairs(&[
            ("redirect_uri", "https://md.phext.io/auth/callback"),
            ("scope", "phext:read phext:write"),
            ("state", "ab cd+ef/gh=="),
        ]);
        let parsed = parse_pairs(&pairs.replace('&', "&")); // same string, parsed back
        assert_eq!(
            parsed.get("redirect_uri").map(|s| s.as_str()),
            Some("https://md.phext.io/auth/callback")
        );
        assert_eq!(
            parsed.get("scope").map(|s| s.as_str()),
            Some("phext:read phext:write")
        );
        assert_eq!(parsed.get("state").map(|s| s.as_str()), Some("ab cd+ef/gh=="));
        // A real callback query string, as the provider builds it.
        let cb = parse_pairs("code=abc123&state=ff00&iss=https%3A%2F%2Fedit.phext.io");
        assert_eq!(cb.get("code").map(|s| s.as_str()), Some("abc123"));
        assert_eq!(cb.get("iss").map(|s| s.as_str()), Some("https://edit.phext.io"));
    }

    #[test]
    fn vault_names_are_ours_and_legal() {
        let name = new_vault_name().unwrap();
        assert!(valid_vault_name(&name), "{name}");
        assert!(name.starts_with("mdwiz-vault-"));
        // The provider's phext-name rule ([A-Za-z0-9_-], ≤128),
        // checked byte by byte.
        assert!(
            name.len() <= 128
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "{name}"
        );
        assert!(!valid_vault_name("choose-your-own-adventure"));
        assert!(!valid_vault_name("mdwiz-vault-"));
        assert!(!valid_vault_name("mdwiz-vault-has space"));
    }

    #[test]
    fn manifests_round_trip() {
        let vault = VaultManifest {
            name: "mdwiz-vault-test1234".to_string(),
            label: "Field notes".to_string(),
            created: 1_763_000_000,
            bundles: vec![
                VaultBundleRef { slug: "notes".into(), n: 1, rev: 3, updated: 1_763_000_100 },
                VaultBundleRef { slug: "essays".into(), n: 2, rev: 1, updated: 1_763_000_200 },
            ],
        };
        let parsed = VaultManifest::parse(&vault.name, &vault.render()).unwrap();
        assert_eq!(parsed.label, "Field notes");
        assert_eq!(parsed.bundles.len(), 2);
        assert_eq!(parsed.bundle("essays").unwrap().n, 2);
        assert_eq!(parsed.next_n(), 3);
        // A manifest naming a different vault is not ours.
        assert!(VaultManifest::parse("mdwiz-vault-other99", &vault.render()).is_none());
        // Plain text is not a manifest.
        assert!(VaultManifest::parse(&vault.name, "just some prose\n").is_none());

        let mut bundle = BundleManifest::new("notes");
        bundle.rev = 3;
        bundle.updated = 1_763_000_100;
        bundle.files = vec![
            VaultFile { path: "index.md".into(), m: 1, rev: 2, sha256: "aa".into() },
            VaultFile { path: "chapters/one fine day.md".into(), m: 2, rev: 1, sha256: "bb".into() },
        ];
        let parsed = BundleManifest::parse(&bundle.render()).unwrap();
        assert_eq!(parsed.slug, "notes");
        assert_eq!(parsed.files.len(), 2);
        // Paths with spaces survive: the path is the rest of the line.
        assert_eq!(
            parsed.file("chapters/one fine day.md").unwrap().m,
            2
        );
        assert_eq!(parsed.next_m(), 3);
        // The starter vault file is a valid one-scroll phext: the
        // manifest text itself.
        let starter = VaultManifest {
            name: vault.name.clone(),
            label: vault.label.clone(),
            created: vault.created,
            bundles: vec![],
        };
        assert!(VaultManifest::parse(&vault.name, &starter.render()).is_some());
    }

    fn lf(hash: &str, base_rev: u64, base_hash: &str) -> LocalFile {
        LocalFile {
            path: "a.md".into(),
            hash: hash.into(),
            text: None,
            base_rev,
            base_hash: base_hash.into(),
        }
    }

    fn vf(rev: u64, hash: &str) -> VaultFile {
        VaultFile { path: "a.md".into(), m: 1, rev, sha256: hash.into() }
    }

    #[test]
    fn push_plans_cover_the_conflict_table() {
        // Birth: no vault entry, no base → write.
        assert_eq!(plan_push(&lf("h1", 0, ""), None, false), Plan::Write);
        // Agreeing contents → skip, whatever the revisions say.
        assert_eq!(plan_push(&lf("h1", 1, "h0"), Some(&vf(1, "h1")), false), Plan::Skip);
        // Only local moved → write.
        assert_eq!(plan_push(&lf("h2", 1, "h1"), Some(&vf(1, "h1")), false), Plan::Write);
        // Only the vault moved → leave it and say so.
        assert_eq!(
            plan_push(&lf("h1", 1, "h1"), Some(&vf(2, "h9")), false),
            Plan::VaultNewer
        );
        // Both moved → conflict; a named force turns it into a write.
        assert_eq!(
            plan_push(&lf("h2", 1, "h1"), Some(&vf(2, "h9")), false),
            Plan::Conflict
        );
        assert_eq!(
            plan_push(&lf("h2", 1, "h1"), Some(&vf(2, "h9")), true),
            Plan::Write
        );
        // The vault lost a file local edited → conflict, not a
        // silent re-upload; local standing still → forget the base.
        assert_eq!(plan_push(&lf("h2", 1, "h1"), None, false), Plan::Conflict);
        assert_eq!(plan_push(&lf("h1", 1, "h1"), None, false), Plan::Skip);
    }

    #[test]
    fn pull_plans_mirror_the_table() {
        // A file this browser lacks → take it.
        assert_eq!(plan_pull(None, &vf(1, "h1"), false), PullPlan::Send);
        // Vault moved, local stood still → take it.
        assert_eq!(
            plan_pull(Some(&lf("h1", 1, "h1")), &vf(2, "h9"), false),
            PullPlan::Send
        );
        // Local moved, vault stood still → keep local.
        assert_eq!(
            plan_pull(Some(&lf("h2", 1, "h1")), &vf(1, "h1"), false),
            PullPlan::Keep
        );
        // Both moved → conflict until the user says take.
        assert_eq!(
            plan_pull(Some(&lf("h2", 1, "h1")), &vf(2, "h9"), false),
            PullPlan::Conflict
        );
        assert_eq!(
            plan_pull(Some(&lf("h2", 1, "h1")), &vf(2, "h9"), true),
            PullPlan::Send
        );
    }

    #[test]
    fn coordinates_stay_inside_the_scheme() {
        assert_eq!(bundle_manifest_coord(3), "1.2.3/1.1.1/1.1.1");
        assert_ne!(bundle_manifest_coord(1), VAULT_MANIFEST_COORD);
        assert_eq!(file_coord(3, 7), "2.1.3/1.1.7/1.1.1");
        assert_eq!(VAULT_MANIFEST_COORD, "1.1.1/1.1.1/1.1.1");
    }

    #[test]
    fn sessions_expire_at_the_cookie_age() {
        let dir = tempfile::tempdir().unwrap();
        let fed = Fed::open(
            dir.path(),
            FedConfig {
                enabled: true,
                issuer: "https://issuer.example".to_string(),
            },
        )
        .unwrap();
        let mut headers = Map::new();
        headers.insert(
            "cookie".to_string(),
            format!("{SESSION_COOKIE}=abc123"),
        );
        let session_at = |created_at: u64| Session {
            access_token: "a".to_string(),
            refresh_token: "r".to_string(),
            expires_at: now_unix() + 3600,
            vault_name: None,
            vault_label: None,
            created_at,
        };
        // Born in the distant past: the record must not answer,
        // however fresh its access token looks — the cookie's
        // Max-Age is the session's age, enforced server-side too.
        fed.state
            .lock()
            .unwrap()
            .sessions
            .insert("abc123".to_string(), session_at(1));
        assert!(fed.session(&headers).is_none());
        // Born now: answers.
        fed.state
            .lock()
            .unwrap()
            .sessions
            .insert("abc123".to_string(), session_at(now_unix()));
        assert!(fed.session(&headers).is_some());
    }

    #[test]
    fn labels_are_tamed_for_manifest_lines() {
        assert_eq!(clean_label("  My\nvault\t "), "My vault");
        assert_eq!(clean_label(""), "My Markdown Wizard vault");
        assert!(clean_label(&"x".repeat(200)).chars().count() <= 80);
    }
}
