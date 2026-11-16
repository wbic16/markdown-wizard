//! markdown-wizard — the md.phext.io server.
//!
//! Phext, stripped to first principles. A bundle is a zip of markdown
//! files: plain text inside an archive anyone can open anywhere. The
//! paths inside the zip are the addresses. Editing is local-first, in
//! one self-contained page; publishing is an atomic swap of the whole
//! bundle (holo-phext's commit discipline, distilled — see store.rs);
//! every published file then lives at two stable addresses — its raw
//! `.md`, which serves the source itself, and its reading view.
//!
//! The HTTP machinery is phext-lattice's, distilled (see http.rs):
//! std-only, one thread per connection, hand-written responses.
//!
//! Usage: markdown-wizard [--dir <data-dir>] [--port 8090] [--host 127.0.0.1]

mod bundle;
mod http;
mod mcp;
mod render;
mod rss;
mod store;
mod work;

use std::collections::HashMap;
use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bundle::{is_markdown_path, is_vault_path, valid_bundle_path, valid_slug, MAX_ZIP_BYTES};
use http::{CacheClass, HTML, JSON, TEXT};
use store::{PublishError, PublishOutcome, Store};
use work::WorkStore;

const EDITOR_HTML: &str = include_str!("../web/editor.html");
const FAVICON_SVG: &str = include_str!("../web/favicon.svg");
const ROBOTS_TXT: &str = "User-agent: *\nAllow: /\n";
const RENDER_CAP: usize = 1024 * 1024;

struct Config {
    dir: PathBuf,
    host: String,
    port: u16,
}

fn parse_args() -> Config {
    let mut cfg = Config {
        dir: PathBuf::from("mdwiz-data"),
        host: "127.0.0.1".to_string(),
        port: 8090,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                i += 1;
                if i < args.len() {
                    cfg.dir = PathBuf::from(&args[i]);
                }
            }
            "--port" => {
                i += 1;
                if i < args.len() {
                    cfg.port = args[i].parse().unwrap_or(cfg.port);
                }
            }
            "--host" => {
                i += 1;
                if i < args.len() {
                    cfg.host = args[i].clone();
                }
            }
            "--help" | "-h" => {
                println!("usage: markdown-wizard [--dir <data-dir>] [--port 8090] [--host 127.0.0.1]");
                std::process::exit(0);
            }
            other => {
                eprintln!("markdown-wizard: unknown argument {other:?} (try --help)");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    cfg
}

fn main() {
    let cfg = parse_args();
    let store = match Store::open(cfg.dir.clone()) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("markdown-wizard: cannot open data dir {}: {e}", cfg.dir.display());
            std::process::exit(1);
        }
    };
    let work = match WorkStore::open(cfg.dir.clone()) {
        Ok(w) => Arc::new(w),
        Err(e) => {
            eprintln!("markdown-wizard: cannot open working copies in {}: {e}", cfg.dir.display());
            std::process::exit(1);
        }
    };
    let listener = TcpListener::bind(format!("{}:{}", cfg.host, cfg.port)).unwrap_or_else(|e| {
        eprintln!("markdown-wizard: cannot bind {}:{}: {e}", cfg.host, cfg.port);
        std::process::exit(1);
    });
    let local = listener.local_addr().expect("bound listener has an address");
    println!(
        "markdown-wizard {} listening on http://{} ({} bundle(s), data dir {})",
        env!("CARGO_PKG_VERSION"),
        local,
        store.count(),
        cfg.dir.display()
    );
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let store = store.clone();
                let work = work.clone();
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                    handle(&mut stream, &store, &work);
                });
            }
            Err(_) => continue,
        }
    }
}

/// The public origin for absolute links (feeds), from the proxy's
/// word for the scheme and the Host header.
fn public_base(headers: &HashMap<String, String>) -> String {
    let scheme = if http::forwarded_https(headers) { "https" } else { "http" };
    let host = headers
        .get("host")
        .cloned()
        .unwrap_or_else(|| "md.phext.io".to_string());
    format!("{scheme}://{host}")
}

fn handle(stream: &mut TcpStream, store: &Arc<Store>, work: &Arc<WorkStore>) {
    let cloned = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(cloned);
    let (method, target, headers) = match http::parse_request(&mut reader) {
        Some(v) => v,
        None => return,
    };
    let head_only = method == "HEAD";
    let path = target.split('?').next().unwrap_or("/").to_string();

    match (method.as_str(), path.as_str()) {
        ("GET" | "HEAD", "/") => {
            http::send_response(stream, 200, HTML, EDITOR_HTML.as_bytes(), CacheClass::Shell, &headers, head_only)
        }
        ("GET" | "HEAD", "/favicon.svg") | ("GET" | "HEAD", "/favicon.ico") => {
            http::send_response(stream, 200, "image/svg+xml", FAVICON_SVG.as_bytes(), CacheClass::Static, &headers, head_only)
        }
        ("GET" | "HEAD", "/robots.txt") => {
            http::send_response(stream, 200, TEXT, ROBOTS_TXT.as_bytes(), CacheClass::Static, &headers, head_only)
        }
        ("GET" | "HEAD", "/api/status") => {
            let body = format!(
                "{{\"ok\":true,\"version\":\"{}\",\"bundles\":{}}}",
                env!("CARGO_PKG_VERSION"),
                store.count()
            );
            http::send_response(stream, 200, JSON, body.as_bytes(), CacheClass::NoStore, &headers, head_only)
        }
        ("POST", "/api/render") => {
            let body = match http::read_body(&mut reader, &headers, RENDER_CAP) {
                Some(b) => b,
                None => return json_error(stream, &headers, 413, "markdown larger than the render limit"),
            };
            let md = String::from_utf8_lossy(&body);
            let fragment = render::render_markdown(&md);
            http::send_response(stream, 200, HTML, fragment.as_bytes(), CacheClass::NoStore, &headers, false)
        }
        ("POST", "/api/publish") => handle_publish(stream, &mut reader, &headers, store),
        ("POST", "/api/restore") => handle_restore(stream, &headers, store),
        // The MCP endpoint: POST is the whole transport; any other
        // method is answered inside, as a JSON 405 (see mcp.rs).
        ("POST", "/mcp" | "/mcp/") | ("GET" | "HEAD", "/mcp" | "/mcp/") => {
            mcp::handle_mcp(stream, &mut reader, &method, &headers, store, work)
        }
        ("GET" | "HEAD", p) if p.starts_with("/api/bundles/") => {
            let slug = &p["/api/bundles/".len()..];
            match store.get(slug) {
                Some(bundle) => {
                    let files: Vec<String> = bundle
                        .parsed
                        .files
                        .iter()
                        .map(|f| format!("\"{}\"", http::json_escape(&f.path)))
                        .collect();
                    let changed = bundle
                        .changed
                        .as_ref()
                        .map(|c| format!("\"{}\"", http::json_escape(c)))
                        .unwrap_or_else(|| "null".to_string());
                    let body = format!(
                        "{{\"ok\":true,\"slug\":\"{}\",\"title\":\"{}\",\"version\":{},\"published_at\":{},\"changed\":{},\"has_previous\":{},\"files\":[{}]}}",
                        http::json_escape(&bundle.slug),
                        http::json_escape(&bundle::bundle_title(&bundle.parsed, &bundle.slug)),
                        bundle.version,
                        bundle.published_at,
                        changed,
                        store.has_previous(slug),
                        files.join(",")
                    );
                    http::send_response(stream, 200, JSON, body.as_bytes(), CacheClass::NoStore, &headers, head_only)
                }
                None => json_error(stream, &headers, 404, "no such bundle"),
            }
        }
        ("GET" | "HEAD", p) if p.starts_with("/b/") => {
            handle_bundle_path(stream, &headers, store, p, head_only)
        }
        ("GET" | "HEAD", _) => {
            let page = render::not_found_page(&path);
            http::send_response(stream, 404, HTML, page.as_bytes(), CacheClass::NoStore, &headers, head_only)
        }
        _ => {
            http::send_response(stream, 405, TEXT, b"method not allowed", CacheClass::NoStore, &headers, false)
        }
    }
}

fn json_error(stream: &mut TcpStream, headers: &HashMap<String, String>, status: u16, msg: &str) -> () {
    let body = format!("{{\"ok\":false,\"error\":\"{}\"}}", http::json_escape(msg));
    http::send_response(stream, status, JSON, body.as_bytes(), CacheClass::NoStore, headers, false)
}

fn handle_publish(
    stream: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    headers: &HashMap<String, String>,
    store: &Arc<Store>,
) {
    let slug = headers.get("x-slug").cloned().unwrap_or_default();
    if !valid_slug(&slug) {
        return json_error(stream, headers, 400, "a slug is 1-64 chars of lowercase letters, digits, and hyphens, starting and ending with a letter or digit");
    }
    let body = match http::read_body(reader, headers, MAX_ZIP_BYTES) {
        Some(b) => b,
        None => return json_error(stream, headers, 413, "bundle larger than the 16 MiB limit"),
    };
    let key = headers.get("x-edit-key").map(|s| s.as_str());
    let changed = headers
        .get("x-changed")
        .and_then(|s| store::clean_changed_line(s));
    match store.publish(&slug, body, key, changed) {
        Ok(PublishOutcome::Created { bundle, edit_key }) => {
            let body = publish_json(&bundle, Some(&edit_key));
            http::send_response(stream, 201, JSON, body.as_bytes(), CacheClass::NoStore, headers, false)
        }
        Ok(PublishOutcome::Replaced { bundle }) => {
            let body = publish_json(&bundle, None);
            http::send_response(stream, 200, JSON, body.as_bytes(), CacheClass::NoStore, headers, false)
        }
        Err(PublishError::Invalid(msg)) => json_error(stream, headers, 422, &msg),
        Err(PublishError::WrongKey) => {
            json_error(stream, headers, 403, "that edit key does not open this slug")
        }
        Err(PublishError::NoPrevious) => json_error(stream, headers, 409, "no previous version"),
        Err(PublishError::Io(msg)) => {
            eprintln!("markdown-wizard: publish {slug} failed: {msg}");
            json_error(stream, headers, 500, "the publish could not be stored")
        }
    }
}

fn handle_restore(stream: &mut TcpStream, headers: &HashMap<String, String>, store: &Arc<Store>) {
    let slug = headers.get("x-slug").cloned().unwrap_or_default();
    if !valid_slug(&slug) {
        return json_error(stream, headers, 400, "invalid slug");
    }
    let key = headers.get("x-edit-key").map(|s| s.as_str());
    match store.restore(&slug, key) {
        Ok(bundle) => {
            let body = publish_json(&bundle, None);
            http::send_response(stream, 200, JSON, body.as_bytes(), CacheClass::NoStore, headers, false)
        }
        Err(PublishError::WrongKey) => json_error(stream, headers, 403, "that edit key does not open this slug"),
        Err(PublishError::NoPrevious) => json_error(stream, headers, 409, "no previous version to restore"),
        Err(PublishError::Invalid(msg)) => json_error(stream, headers, 422, &msg),
        Err(PublishError::Io(msg)) => {
            eprintln!("markdown-wizard: restore {slug} failed: {msg}");
            json_error(stream, headers, 500, "the restore could not be stored")
        }
    }
}

fn publish_json(bundle: &store::Bundle, edit_key: Option<&str>) -> String {
    let files: Vec<String> = bundle
        .parsed
        .files
        .iter()
        .map(|f| format!("\"{}\"", http::json_escape(&f.path)))
        .collect();
    let key_field = edit_key
        .map(|k| format!(",\"edit_key\":\"{}\"", http::json_escape(k)))
        .unwrap_or_default();
    format!(
        "{{\"ok\":true,\"slug\":\"{}\",\"version\":{},\"published_at\":{},\"url\":\"/b/{}/\",\"files\":[{}]{}}}",
        http::json_escape(&bundle.slug),
        bundle.version,
        bundle.published_at,
        http::json_escape(&bundle.slug),
        files.join(","),
        key_field
    )
}

/// Everything under `/b/`: a bundle's public life.
fn handle_bundle_path(
    stream: &mut TcpStream,
    headers: &HashMap<String, String>,
    store: &Arc<Store>,
    path: &str,
    head_only: bool,
) {
    let rest = &path["/b/".len()..];

    // The bundle itself, to take away: /b/<slug>.zip
    if !rest.contains('/') && rest.ends_with(".zip") {
        let slug = &rest[..rest.len() - 4];
        if let Some(bundle) = store.get(slug) {
            let extra = format!(
                "Content-Disposition: attachment; filename=\"{slug}.zip\"\r\n"
            );
            http::send_response_extra(
                stream,
                200,
                "application/zip",
                &bundle.parsed.zip_bytes,
                CacheClass::Shell,
                headers,
                head_only,
                &extra,
            );
            return;
        }
    }

    let (slug, sub) = match rest.split_once('/') {
        Some((s, sub)) => (s, sub),
        None => {
            // /b/<slug> → the bundle's front door, with its slash.
            if store.get(rest).is_some() {
                http::send_redirect(stream, &format!("/b/{rest}/"), headers);
            } else {
                let page = render::not_found_page(path);
                http::send_response(stream, 404, HTML, page.as_bytes(), CacheClass::NoStore, headers, head_only);
            }
            return;
        }
    };
    let bundle = match store.get(slug) {
        Some(b) => b,
        None => {
            let page = render::not_found_page(path);
            http::send_response(stream, 404, HTML, page.as_bytes(), CacheClass::NoStore, headers, head_only);
            return;
        }
    };
    let sub = http::percent_decode(sub);
    let base = public_base(headers);

    if sub.is_empty() {
        let page = render::index_page(&bundle, &base);
        http::send_response(stream, 200, HTML, page.as_bytes(), CacheClass::Shell, headers, head_only);
        return;
    }
    if sub == "feed.xml" {
        let feed = rss::feed_xml(&bundle, &base);
        http::send_response(stream, 200, "application/rss+xml; charset=utf-8", feed.as_bytes(), CacheClass::Shell, headers, head_only);
        return;
    }
    // The vault is private by structure: it is never served.
    if is_vault_path(&sub) || !valid_bundle_path(&sub) {
        let page = render::not_found_page(path);
        http::send_response(stream, 404, HTML, page.as_bytes(), CacheClass::NoStore, headers, head_only);
        return;
    }
    // The pattern: the .md address serves the source itself.
    if is_markdown_path(&sub) {
        if let Some(f) = bundle.parsed.file(&sub) {
            http::send_response(stream, 200, "text/markdown; charset=utf-8", &f.bytes, CacheClass::Shell, headers, head_only);
            return;
        }
    }
    // A carried file at its exact address.
    if let Some(f) = bundle.parsed.file(&sub) {
        http::send_response(stream, 200, content_type_for(&sub), &f.bytes, CacheClass::Shell, headers, head_only);
        return;
    }
    // The reading view: the same address without the extension.
    for ext in [".md", ".markdown"] {
        let candidate = format!("{sub}{ext}");
        if let Some(f) = bundle.parsed.file(&candidate) {
            let page = render::reading_page(&bundle, f);
            http::send_response(stream, 200, HTML, page.as_bytes(), CacheClass::Shell, headers, head_only);
            return;
        }
    }
    let page = render::not_found_page(path);
    http::send_response(stream, 404, HTML, page.as_bytes(), CacheClass::NoStore, headers, head_only);
}

fn content_type_for(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".txt") {
        "text/plain; charset=utf-8"
    } else if lower.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if lower.ends_with(".json") {
        "application/json; charset=utf-8"
    } else if lower.ends_with(".pdf") {
        "application/pdf"
    } else {
        "application/octet-stream"
    }
}
