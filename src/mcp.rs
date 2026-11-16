//! The MCP endpoint — an agent-facing surface for the same bundles
//! the site serves, distilled from phext-lattice's native MCP
//! server (`src/bin/phext_edit.rs`, its PR #41) the way this
//! service's HTTP layer was distilled from the same file.
//!
//! What was taken: the transport shape. Stateless streamable HTTP —
//! POST is the whole transport (another method is answered 405, so
//! a probing client learns the endpoint is alive rather than
//! dead), JSON-RPC 2.0 bodies, batch arrays answered as batches,
//! notifications answered with silence (202), the `initialize`
//! negotiation over the published protocol versions, an
//! `MCP-Protocol-Version` header the server does not speak refused
//! with 400 rather than silently honored, tool results wrapped as
//! text content carrying pretty JSON, and refusals reported as
//! `isError` tool results rather than transport failures.
//!
//! What was deliberately NOT taken: the OAuth 2.1 layer (the
//! lattice's PR #42). At this scale the bundle's edit key — the
//! credential the web publish already issues — is the whole auth
//! story. It travels in the `X-Edit-Key` header, exactly as it does
//! for `/api/publish`, or as a per-call `edit_key` argument for
//! clients that cannot set headers. Published bundles are public
//! to read, exactly as the site itself is; drafts (working copies)
//! and every write require the key.
//!
//! "Stream edits" at this scale: a client fires a sequence of
//! `write_file` calls — singly, or batched in one JSON-RPC array —
//! and each lands in the slug's working copy immediately and
//! durably (see work.rs). `publish_bundle` then commits the whole
//! draft through the store's own publish path: the same gate, the
//! same atomic swap, the same version increment as a web release.
//! The WordPress push rides that commit too, through the same hook
//! the web release uses: after the commit lands, never failing it,
//! its per-file report persisted with the bundle and carried in the
//! publish result. An agent's edits and a human's edits meet only
//! at that commit point, never in a half-published state.

use std::collections::HashMap;
use std::io::BufReader;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::bundle::{
    bundle_title, is_vault_path, valid_bundle_path, valid_slug, BundleFile,
};
use crate::http::{self, CacheClass};
use crate::cloud::CloudStore;
use crate::store::{clean_changed_line, Store};
use crate::wordpress::WpStore;
use crate::work::{diverged, WorkError, WorkStore, WorkingCopy};

const MCP_PROTOCOL_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
/// One request may carry a whole file (16 MiB, the bundle cap) plus
/// JSON escaping and its envelope; 32 MiB is the round bound.
const MCP_MAX_BODY: usize = 32 * 1024 * 1024;
const SEARCH_HIT_CAP: usize = 50;

const WRONG_KEY: &str = "that edit key does not open this slug";

pub fn handle_mcp(
    stream: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    method: &str,
    headers: &HashMap<String, String>,
    store: &Arc<Store>,
    work: &Arc<WorkStore>,
    wp: &Arc<WpStore>,
    cloudstore: &Arc<CloudStore>,
) {
    // Stateless: there is no server-initiated stream for a GET to
    // open and no transport session for a DELETE to close. A
    // non-POST probe gets the spec-permitted 405, in JSON.
    if method != "POST" {
        send_json(stream, headers, 405, r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"method not allowed: POST is the whole transport"}}"#);
        return;
    }
    let content_type = headers.get("content-type").map(String::as_str).unwrap_or("");
    if !content_type.starts_with("application/json") {
        send_json(stream, headers, 415, r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"unsupported media type: application/json required"}}"#);
        return;
    }
    // MCP-Protocol-Version, when a client sends one, names the
    // version negotiated at initialize. A version this server does
    // not speak is refused with 400 rather than silently honored
    // (the lattice's rule, from the 2025-06-18 transport spec).
    if let Some(version) = headers.get("mcp-protocol-version") {
        const KNOWN: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
        if !KNOWN.contains(&version.trim()) {
            send_json(stream, headers, 400, r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"unsupported MCP protocol version"}}"#);
            return;
        }
    }
    let body = match http::read_body(reader, headers, MCP_MAX_BODY) {
        Some(b) => b,
        None => {
            send_json(stream, headers, 413, r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"request body larger than the limit"}}"#);
            return;
        }
    };
    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            send_json(stream, headers, 400, r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#);
            return;
        }
    };
    let http_key = headers.get("x-edit-key").map(String::as_str);
    // A batch is answered as a batch, notifications omitted.
    if let Value::Array(items) = &value {
        let replies: Vec<Value> = items
            .iter()
            .filter_map(|item| dispatch(store, work, wp, cloudstore, item, http_key))
            .collect();
        if replies.is_empty() {
            http::send_response(stream, 202, http::JSON, b"", CacheClass::NoStore, headers, false);
        } else {
            let out = serde_json::to_string(&replies).unwrap_or_else(|_| "[]".into());
            send_json(stream, headers, 200, &out);
        }
        return;
    }
    match dispatch(store, work, wp, cloudstore, &value, http_key) {
        Some(reply) => {
            let out = serde_json::to_string(&reply).unwrap_or_else(|_| "{}".into());
            send_json(stream, headers, 200, &out);
        }
        None => {
            http::send_response(stream, 202, http::JSON, b"", CacheClass::NoStore, headers, false)
        }
    }
}

fn send_json(stream: &mut TcpStream, headers: &HashMap<String, String>, status: u16, body: &str) {
    http::send_response(stream, status, http::JSON, body.as_bytes(), CacheClass::NoStore, headers, false);
}

/// One JSON-RPC request in, its response out — or None for a
/// notification, which is answered by silence.
fn dispatch(
    store: &Arc<Store>,
    work: &Arc<WorkStore>,
    wp: &Arc<WpStore>,
    cloudstore: &Arc<CloudStore>,
    req: &Value,
    http_key: Option<&str>,
) -> Option<Value> {
    if !req.is_object() {
        return Some(json!({
            "jsonrpc": "2.0", "id": null,
            "error": { "code": -32600, "message": "invalid request" },
        }));
    }
    let id = req.get("id")?.clone();
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let outcome: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => Ok(call_tool(store, work, wp, cloudstore, &params, http_key)),
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": code, "message": message },
        }),
    })
}

fn initialize(params: &Value) -> Value {
    let asked = params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let version = if MCP_PROTOCOL_VERSIONS.contains(&asked) {
        asked
    } else {
        MCP_PROTOCOL_VERSION
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "markdown-wizard",
            "title": "Markdown Wizard",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "Tools for the markdown bundles hosted at md.phext.io. A bundle is a \
            zip of markdown files; the paths inside it are the addresses, and publishing \
            swaps the whole bundle atomically at its stable URLs. Start with list_bundles, \
            then list_files, read_file, and search_bundle — published bundles are public \
            to read. To edit: write_file lands files in the slug's server-side working \
            copy (draft=true reads it back), and publish_bundle commits the draft. Writes \
            and draft reads require the bundle's edit key, sent as the X-Edit-Key header \
            or the edit_key argument. A first write_file to a never-published slug mints \
            that key and returns it once — keep it. A file written with keep_private=true \
            stays in the working copy but is held out of the publish set: publish_bundle \
            commits everything else, and the result names what was held back. \
            list_files with draft=true shows each file's keep_private mark.",
    })
}

fn tools() -> Value {
    let slug_arg = json!({
        "type": "string",
        "description": "Bundle slug from list_bundles — the bundle's public name, as in md.phext.io/b/<slug>/.",
    });
    let draft_arg = json!({
        "type": "boolean",
        "default": false,
        "description": "Read the slug's working copy (its unpublished draft) instead of the published bundle. Requires the edit key.",
    });
    let key_arg = json!({
        "type": "string",
        "description": "The bundle's edit key. The X-Edit-Key header on the /mcp request also carries it; the argument wins when both are sent.",
    });
    json!([
        {
            "name": "list_bundles",
            "description": "List the bundles published on this server, with title, version, publish time, and file count.",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "list_files",
            "description": "A bundle's file tree: paths and sizes. By default the published bundle (the vault is private and never listed); with draft=true, the working copy, vault included, for the key holder — each file there also carries its keep_private mark.",
            "inputSchema": {
                "type": "object",
                "properties": { "slug": slug_arg, "draft": draft_arg, "edit_key": key_arg },
                "required": ["slug"],
            },
        },
        {
            "name": "read_file",
            "description": "Read one file's text by slug and path — the published bundle by default, the working copy with draft=true (edit key required).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slug": slug_arg,
                    "path": { "type": "string", "description": "The file's address inside the bundle — for example essays/one.md." },
                    "draft": draft_arg,
                    "edit_key": key_arg,
                },
                "required": ["slug", "path"],
            },
        },
        {
            "name": "search_bundle",
            "description": "Search a bundle's text for a substring; returns up to 50 hits with path, line number, and the matching line. Published bundle by default; draft=true searches the working copy (edit key required).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slug": slug_arg,
                    "query": { "type": "string", "description": "Text to find (a plain substring, matched per line)." },
                    "draft": draft_arg,
                    "edit_key": key_arg,
                },
                "required": ["slug", "query"],
            },
        },
        {
            "name": "write_file",
            "description": "Create or replace one file in the slug's working copy. Opens the copy when needed: seeded from the published bundle (edit key required), or minted fresh for a never-published slug — that first call returns the slug's new edit key, once. Nothing is public until publish_bundle. The keep_private argument sets or clears the file's keep-private mark: a marked file stays in the working copy but is excluded from the set publish_bundle commits, so it appears in no published page, feed, download, or sync; marking a file that is already live and re-publishing removes it from the published bundle.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slug": slug_arg,
                    "path": { "type": "string", "description": "The file's address inside the bundle — for example essays/one.md." },
                    "content": { "type": "string", "description": "The file's new text (may be empty to clear it)." },
                    "keep_private": { "type": "boolean", "description": "Set (true) or clear (false) this file's keep-private mark with this write. Omit to leave the mark as it is." },
                    "edit_key": key_arg,
                },
                "required": ["slug", "path", "content"],
            },
        },
        {
            "name": "publish_bundle",
            "description": "Commit the slug's working copy atomically — the same publish as the web release: version increments, the URL never changes, the previous version is kept for restore. Files marked keep-private are held out of the commit and named in the result's kept_private field. Refuses when the published bundle has moved past the draft's base version. Requires the edit key. If the bundle is connected to a WordPress site, the release is pushed there too and the per-file sync report rides the result as a wordpress field. If it is connected to cloud storage (an S3 bucket, an Azure container, or a Google Cloud bucket), the release is copied there too and the report rides as a cloud field.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slug": slug_arg,
                    "changed": { "type": "string", "description": "An optional one-line 'what changed?' note; it appears on the published pages and in the feed." },
                    "edit_key": key_arg,
                },
                "required": ["slug"],
            },
        },
    ])
}

fn call_tool(
    store: &Arc<Store>,
    work: &Arc<WorkStore>,
    wp: &Arc<WpStore>,
    cloudstore: &Arc<CloudStore>,
    params: &Value,
    http_key: Option<&str>,
) -> Value {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    // The per-call argument wins; the header is the session's key.
    let key = args
        .get("edit_key")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or(http_key);
    // A tool must answer, never panic: a refusal is an isError
    // result, and so is an internal failure.
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_tool(store, work, wp, cloudstore, name, &args, key)
    }));
    match ran {
        Ok(Ok(value)) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into());
            json!({ "content": [{ "type": "text", "text": text }], "isError": false })
        }
        Ok(Err(message)) => {
            json!({ "content": [{ "type": "text", "text": message }], "isError": true })
        }
        Err(_) => {
            eprintln!("markdown-wizard: MCP tool {name} panicked; answered as an error");
            json!({
                "content": [{ "type": "text", "text": "internal error: the call failed inside the server; it committed nothing" }],
                "isError": true,
            })
        }
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing argument: {key}"))
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn run_tool(
    store: &Arc<Store>,
    work: &Arc<WorkStore>,
    wp: &Arc<WpStore>,
    cloudstore: &Arc<CloudStore>,
    name: &str,
    args: &Value,
    key: Option<&str>,
) -> Result<Value, String> {
    match name {
        "list_bundles" => Ok(list_bundles(store, work)),
        "list_files" => {
            let slug = arg_str(args, "slug")?;
            list_files(store, work, slug, arg_bool(args, "draft"), key)
        }
        "read_file" => {
            let slug = arg_str(args, "slug")?;
            let path = arg_str(args, "path")?;
            read_file(store, work, slug, path, arg_bool(args, "draft"), key)
        }
        "search_bundle" => {
            let slug = arg_str(args, "slug")?;
            let query = arg_str(args, "query")?;
            search_bundle(store, work, slug, query, arg_bool(args, "draft"), key)
        }
        "write_file" => {
            let slug = arg_str(args, "slug")?;
            let path = arg_str(args, "path")?;
            let content = args
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "missing argument: content".to_string())?;
            // Tri-state: an absent argument leaves the mark alone.
            let keep_private = args.get("keep_private").and_then(|v| v.as_bool());
            write_file(store, work, slug, path, content, key, keep_private)
        }
        "publish_bundle" => {
            let slug = arg_str(args, "slug")?;
            let changed = args
                .get("changed")
                .and_then(|v| v.as_str())
                .and_then(clean_changed_line);
            publish_bundle(work, store, wp, cloudstore, slug, changed, key)
        }
        _ => Err(format!("unknown tool: {name}")),
    }
}

fn list_bundles(store: &Arc<Store>, work: &Arc<WorkStore>) -> Value {
    let bundles: Vec<Value> = store
        .list()
        .iter()
        .map(|b| {
            json!({
                "slug": b.slug,
                "title": bundle_title(&b.parsed, &b.slug),
                "version": b.version,
                "published_at": b.published_at,
                "files": b.parsed.files.iter().filter(|f| !is_vault_path(&f.path)).count(),
                "has_previous": store.has_previous(&b.slug),
                "has_working_copy": work.get(&b.slug).is_some(),
            })
        })
        .collect();
    json!({ "bundles": bundles })
}

/// The draft's lock, with its key verified — the one door every
/// draft read goes through.
fn lock_draft(
    work: &Arc<WorkStore>,
    slug: &str,
    key: Option<&str>,
) -> Result<Arc<Mutex<WorkingCopy>>, String> {
    let copy = work
        .get(slug)
        .ok_or_else(|| format!("no working copy for slug {slug} — a write_file call opens one"))?;
    if !copy.lock().unwrap().verify(key) {
        return Err(WRONG_KEY.to_string());
    }
    Ok(copy)
}

fn file_entry(f: &BundleFile, with_vault_flag: bool) -> Value {
    let mut entry = json!({
        "path": f.path,
        "bytes": f.bytes.len(),
        "markdown": f.is_markdown,
    });
    if with_vault_flag {
        entry["vault"] = json!(is_vault_path(&f.path));
    }
    entry
}

fn list_files(
    store: &Arc<Store>,
    work: &Arc<WorkStore>,
    slug: &str,
    draft: bool,
    key: Option<&str>,
) -> Result<Value, String> {
    if draft {
        let copy = lock_draft(work, slug, key)?;
        let guard = copy.lock().unwrap();
        let files: Vec<Value> = guard
            .files
            .iter()
            .map(|f| {
                let mut entry = file_entry(f, true);
                entry["keep_private"] = json!(guard.is_private(&f.path));
                entry
            })
            .collect();
        return Ok(json!({
            "slug": slug,
            "source": "working",
            "base_version": guard.base_version,
            "published_version": store.get(slug).map(|b| b.version).unwrap_or(0),
            "diverged": diverged(store, slug, &guard),
            "files": files,
        }));
    }
    let bundle = store
        .get(slug)
        .ok_or_else(|| format!("no published bundle with slug {slug}"))?;
    let files: Vec<Value> = bundle
        .parsed
        .files
        .iter()
        .filter(|f| !is_vault_path(&f.path))
        .map(|f| file_entry(f, false))
        .collect();
    Ok(json!({
        "slug": slug,
        "source": "published",
        "title": bundle_title(&bundle.parsed, slug),
        "version": bundle.version,
        "published_at": bundle.published_at,
        "files": files,
    }))
}

fn read_file(
    store: &Arc<Store>,
    work: &Arc<WorkStore>,
    slug: &str,
    path: &str,
    draft: bool,
    key: Option<&str>,
) -> Result<Value, String> {
    if !valid_bundle_path(path) {
        return Err(format!("unsafe path: {path:?}"));
    }
    if draft {
        let copy = lock_draft(work, slug, key)?;
        let guard = copy.lock().unwrap();
        let f = guard
            .file(path)
            .ok_or_else(|| format!("no file {path} in the working copy of {slug}"))?;
        let text = f
            .text()
            .ok_or_else(|| format!("file {path} is not UTF-8 text; read_file returns text"))?;
        return Ok(json!({
            "slug": slug,
            "path": path,
            "source": "working",
            "base_version": guard.base_version,
            "content": text,
        }));
    }
    let bundle = store
        .get(slug)
        .ok_or_else(|| format!("no published bundle with slug {slug}"))?;
    // The vault is private by structure: on the published side it
    // is not distinguished from a file that was never there.
    let f = bundle
        .parsed
        .file(path)
        .filter(|f| !is_vault_path(&f.path))
        .ok_or_else(|| format!("no file {path} in the published bundle {slug}"))?;
    let text = f
        .text()
        .ok_or_else(|| format!("file {path} is not UTF-8 text; read_file returns text"))?;
    Ok(json!({
        "slug": slug,
        "path": path,
        "source": "published",
        "version": bundle.version,
        "content": text,
    }))
}

fn search_bundle(
    store: &Arc<Store>,
    work: &Arc<WorkStore>,
    slug: &str,
    query: &str,
    draft: bool,
    key: Option<&str>,
) -> Result<Value, String> {
    // Collect (path, text) pairs from the chosen source, then walk
    // them uniformly. Published: the vault is excluded. Draft: the
    // key holder searches their own vault too.
    let (source, texts): (&str, Vec<(String, String)>) = if draft {
        let copy = lock_draft(work, slug, key)?;
        let guard = copy.lock().unwrap();
        (
            "working",
            guard
                .files
                .iter()
                .filter_map(|f| f.text().map(|t| (f.path.clone(), t.to_string())))
                .collect(),
        )
    } else {
        let bundle = store
            .get(slug)
            .ok_or_else(|| format!("no published bundle with slug {slug}"))?;
        (
            "published",
            bundle
                .parsed
                .files
                .iter()
                .filter(|f| !is_vault_path(&f.path))
                .filter_map(|f| f.text().map(|t| (f.path.clone(), t.to_string())))
                .collect(),
        )
    };
    let mut hits = Vec::new();
    let mut truncated = false;
    'outer: for (path, text) in &texts {
        for (i, line) in text.lines().enumerate() {
            if line.contains(query) {
                if hits.len() >= SEARCH_HIT_CAP {
                    truncated = true;
                    break 'outer;
                }
                let line_text: String = line.chars().take(200).collect();
                hits.push(json!({ "path": path, "line": i + 1, "text": line_text }));
            }
        }
    }
    Ok(json!({
        "slug": slug,
        "source": source,
        "query": query,
        "hits": hits,
        "truncated": truncated,
    }))
}

fn write_file(
    store: &Arc<Store>,
    work: &Arc<WorkStore>,
    slug: &str,
    path: &str,
    content: &str,
    key: Option<&str>,
    keep_private: Option<bool>,
) -> Result<Value, String> {
    if !valid_slug(slug) {
        return Err("a slug is 1-64 chars of lowercase letters, digits, and hyphens, starting and ending with a letter or digit".to_string());
    }
    if !valid_bundle_path(path) {
        return Err(format!("unsafe path: {path:?}"));
    }
    let info = work
        .write_file(store, slug, path, content.as_bytes(), key, keep_private)
        .map_err(work_error_message)?;
    let mut out = json!({
        "slug": slug,
        "path": path,
        "bytes": content.len(),
        "source": "working",
        "files": info.files,
        "base_version": info.base_version,
        "published_version": store.get(slug).map(|b| b.version).unwrap_or(0),
        "diverged": info.diverged,
        "keep_private": info.keep_private,
    });
    if let Some(minted) = &info.minted_key {
        out["edit_key"] = json!(minted);
        out["note"] = json!(format!(
            "This edit key was minted for slug {slug} and is shown once: it opens this slug's working copy and, after the first publish, the bundle itself. Keep it — it cannot be recovered."
        ));
    }
    Ok(out)
}

fn publish_bundle(
    work: &Arc<WorkStore>,
    store: &Arc<Store>,
    wp: &Arc<WpStore>,
    cloudstore: &Arc<CloudStore>,
    slug: &str,
    changed: Option<String>,
    key: Option<&str>,
) -> Result<Value, String> {
    if !valid_slug(slug) {
        return Err("a slug is 1-64 chars of lowercase letters, digits, and hyphens, starting and ending with a letter or digit".to_string());
    }
    let info = work
        .publish(store, slug, key, changed)
        .map_err(|e| match e {
            WorkError::NoWorkingCopy => format!(
                "no working copy for slug {slug} — there is nothing to publish; a write_file call opens one"
            ),
            other => work_error_message(other),
        })?;
    let mut out = json!({
        "slug": slug,
        "version": info.version,
        "published_at": info.published_at,
        "url": format!("/b/{slug}/"),
        "files": info.files,
    });
    // What the publish held back, named when there is any: the
    // marked files stayed in the working copy and are in no
    // published surface.
    if !info.kept_private.is_empty() {
        out["kept_private"] = json!(info.kept_private);
    }
    // The WordPress push rides an MCP publish exactly as it rides a
    // web one — the same hook, run after the commit has landed,
    // never failing the publish. Its report is persisted with the
    // bundle by the sync itself and carried here in the same shape
    // the web publish response carries: a `wordpress` field.
    if let Some(bundle) = store.get(slug) {
        if let Some(report) = crate::wordpress::sync_if_configured(wp, &bundle) {
            if let Ok(value) = serde_json::from_str::<Value>(&report.to_json()) {
                out["wordpress"] = value;
            }
        }
        // The cloud sync rides the same hook, in the same shape:
        // a `cloud` field on the result, present only when a cloud
        // connection (S3, Azure, or GCS) is configured.
        if let Some(report) = crate::cloud::sync_if_configured(cloudstore, &bundle) {
            if let Ok(value) = serde_json::from_str::<Value>(&crate::cloud::report_json(&report)) {
                out["cloud"] = value;
            }
        }
    }
    Ok(out)
}

fn work_error_message(e: WorkError) -> String {
    match e {
        WorkError::WrongKey => WRONG_KEY.to_string(),
        WorkError::NoWorkingCopy => "no working copy for this slug".to_string(),
        WorkError::Invalid(m) => m,
        WorkError::Stale { base, current } => format!(
            "the published bundle has moved to version {current} since this working copy was seeded from version {base}; read the current files and re-apply the edits before publishing — a stale draft never overwrites a newer release"
        ),
        WorkError::Io(m) => {
            eprintln!("markdown-wizard: MCP store failure: {m}");
            "the change could not be stored".to_string()
        }
    }
}
