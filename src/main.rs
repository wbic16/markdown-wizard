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
mod cloud;
mod fed;
mod http;
mod mcp;
mod outputs;
mod render;
mod rss;
mod store;
mod wordpress;
mod work;

use std::collections::HashMap;
use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bundle::{
    constant_time_eq, is_markdown_path, is_vault_path, sha256_hex, valid_bundle_path, valid_slug,
    MAX_ZIP_BYTES,
};
use cloud::CloudStore;
use http::{CacheClass, HTML, JSON, TEXT};
use store::{PublishError, PublishOutcome, Store};
use wordpress::WpStore;
use work::WorkStore;

const EDITOR_HTML: &str = include_str!("../web/editor.html");
const FAVICON_SVG: &str = include_str!("../web/favicon.svg");
const FAVICON_PNG: &[u8] = include_bytes!("../web/favicon.png");

/// The keeper of the place: the Pencil Puffball, chosen by Will
/// (2026-11-15). The artwork of record and the round badge cropped
/// from it — compiled in, like everything else the server serves.
const MASCOT_ARTWORK: &[u8] = include_bytes!("../web/mascot-pencil-puffball.webp");
const MASCOT_BADGE: &[u8] = include_bytes!("../web/mascot-badge.webp");
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
    let wp = Arc::new(WpStore::new(cfg.dir.clone()));
    // Federation state (the OAuth client registration, sessions,
    // and the provider tokens behind them) lives beside the
    // bundles. State that will not open degrades the deployment
    // to local-only rather than taking the editor down with it.
    let fed_config = fed::FedConfig::from_env();
    let fed = match fed::Fed::open(&cfg.dir, fed_config) {
        Ok(f) => Arc::new(f),
        Err(e) => {
            eprintln!(
                "markdown-wizard: federation state unavailable ({e}); continuing local-only"
            );
            let off = fed::FedConfig { enabled: false, ..fed::FedConfig::from_env() };
            Arc::new(fed::Fed::open(&cfg.dir, off).expect("disabled federation state"))
        }
    };
    let cloud = Arc::new(CloudStore::new(cfg.dir.clone()));
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
    if fed.enabled() {
        println!(
            "markdown-wizard federation: sign-in via {} enabled",
            fed.issuer()
        );
    }
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let store = store.clone();
                let work = work.clone();
                let wp = wp.clone();
                let fed = fed.clone();
                let cloud = cloud.clone();
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                    handle(&mut stream, &store, &work, &wp, &fed, &cloud);
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

fn handle(
    stream: &mut TcpStream,
    store: &Arc<Store>,
    work: &Arc<WorkStore>,
    wp: &Arc<WpStore>,
    fed: &Arc<fed::Fed>,
    cloudstore: &Arc<CloudStore>,
) {
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
        ("GET" | "HEAD", "/integrate") => {
            let page = render::integrate_page();
            http::send_response(stream, 200, HTML, page.as_bytes(), CacheClass::Shell, &headers, head_only)
        }
        ("GET" | "HEAD", "/favicon.svg") | ("GET" | "HEAD", "/favicon.ico") => {
            http::send_response(stream, 200, "image/svg+xml", FAVICON_SVG.as_bytes(), CacheClass::Static, &headers, head_only)
        }
        ("GET" | "HEAD", "/favicon.png") => {
            http::send_response(stream, 200, "image/png", FAVICON_PNG, CacheClass::Static, &headers, head_only)
        }
        ("GET" | "HEAD", "/mascot-pencil-puffball.webp") => {
            http::send_response(stream, 200, "image/webp", MASCOT_ARTWORK, CacheClass::Static, &headers, head_only)
        }
        ("GET" | "HEAD", "/mascot-badge.webp") => {
            http::send_response(stream, 200, "image/webp", MASCOT_BADGE, CacheClass::Static, &headers, head_only)
        }
        ("GET" | "HEAD", "/robots.txt") => {
            http::send_response(stream, 200, TEXT, ROBOTS_TXT.as_bytes(), CacheClass::Static, &headers, head_only)
        }
        ("GET" | "HEAD", "/rss.xml") => {
            // The site-wide feed: every published bundle's items,
            // newest first (see rss.rs). A live-instance surface
            // only — the static download carries one bundle's own
            // feed, never this aggregate.
            let bundles = store.list();
            let refs: Vec<&store::Bundle> = bundles.iter().map(|b| b.as_ref()).collect();
            let feed = rss::site_feed_xml(&refs, &public_base(&headers));
            http::send_response(stream, 200, "application/rss+xml; charset=utf-8", feed.as_bytes(), CacheClass::Shell, &headers, head_only);
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
        ("POST", "/api/publish") => handle_publish(stream, &mut reader, &headers, store, wp, cloudstore),
        ("POST", "/api/restore") => handle_restore(stream, &headers, store, wp, cloudstore),
        ("POST", "/api/wordpress/sync") => handle_wp_sync(stream, &headers, store, wp),
        ("POST", "/api/wordpress") => handle_wp_save(stream, &mut reader, &headers, store, wp),
        ("GET" | "HEAD", p) if p.starts_with("/api/wordpress/") => {
            let slug = &p["/api/wordpress/".len()..];
            handle_wp_get(stream, &headers, store, wp, slug, head_only)
        }
        ("POST", "/api/cloud/sync") => handle_cloud_sync(stream, &headers, store, cloudstore),
        ("POST", "/api/cloud") => handle_cloud_save(stream, &mut reader, &headers, store, cloudstore),
        ("GET" | "HEAD", p) if p.starts_with("/api/cloud/") => {
            let slug = &p["/api/cloud/".len()..];
            handle_cloud_get(stream, &headers, store, cloudstore, slug, head_only)
        }
        // The MCP endpoint: POST is the whole transport; any other
        // method is answered inside, as a JSON 405 (see mcp.rs).
        ("POST", "/mcp" | "/mcp/") | ("GET" | "HEAD", "/mcp" | "/mcp/") => {
            mcp::handle_mcp(stream, &mut reader, &method, &headers, store, work, wp, cloudstore)
        }
        // Federation via edit.phext.io (see fed.rs): the browser
        // flow, the session's own endpoints, and the vault sync.
        ("GET", "/auth/login") => fed.handle_login(stream, &headers, &public_base(&headers)),
        ("GET", "/auth/callback") => fed.handle_callback(stream, &headers, &target),
        ("POST", "/auth/logout") => fed.handle_logout(stream, &headers),
        ("GET" | "HEAD", "/api/me") => fed.handle_me(stream, &headers, head_only),
        ("GET" | "HEAD", "/api/vault") => fed.handle_vault_status(stream, &headers, head_only),
        ("POST", "/api/vault/new") => fed.handle_vault_new(stream, &mut reader, &headers),
        ("POST", "/api/vault/choose") => {
            fed.handle_vault_choose(stream, &mut reader, &headers)
        }
        ("GET" | "HEAD", "/api/vault/starter") => {
            fed.handle_vault_starter(stream, &headers, head_only)
        }
        ("POST", "/api/vault/sync") => fed.handle_vault_sync(stream, &mut reader, &headers),
        ("POST", "/api/vault/pull") => fed.handle_vault_pull(stream, &mut reader, &headers),
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
    wp: &Arc<WpStore>,
    cloudstore: &Arc<CloudStore>,
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
            let report = wordpress::sync_if_configured(wp, &bundle);
            let cloud_report = cloud::sync_if_configured(cloudstore, &bundle);
            let body = publish_json(&bundle, Some(&edit_key), report.as_ref(), cloud_report.as_ref());
            http::send_response(stream, 201, JSON, body.as_bytes(), CacheClass::NoStore, headers, false)
        }
        Ok(PublishOutcome::Replaced { bundle }) => {
            let report = wordpress::sync_if_configured(wp, &bundle);
            let cloud_report = cloud::sync_if_configured(cloudstore, &bundle);
            let body = publish_json(&bundle, None, report.as_ref(), cloud_report.as_ref());
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

fn handle_restore(
    stream: &mut TcpStream,
    headers: &HashMap<String, String>,
    store: &Arc<Store>,
    wp: &Arc<WpStore>,
    cloudstore: &Arc<CloudStore>,
) {
    let slug = headers.get("x-slug").cloned().unwrap_or_default();
    if !valid_slug(&slug) {
        return json_error(stream, headers, 400, "invalid slug");
    }
    let key = headers.get("x-edit-key").map(|s| s.as_str());
    match store.restore(&slug, key) {
        Ok(bundle) => {
            let report = wordpress::sync_if_configured(wp, &bundle);
            let cloud_report = cloud::sync_if_configured(cloudstore, &bundle);
            let body = publish_json(&bundle, None, report.as_ref(), cloud_report.as_ref());
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

fn publish_json(
    bundle: &store::Bundle,
    edit_key: Option<&str>,
    report: Option<&wordpress::SyncReport>,
    cloud_report: Option<&cloud::CloudReport>,
) -> String {
    let files: Vec<String> = bundle
        .parsed
        .files
        .iter()
        .map(|f| format!("\"{}\"", http::json_escape(&f.path)))
        .collect();
    let key_field = edit_key
        .map(|k| format!(",\"edit_key\":\"{}\"", http::json_escape(k)))
        .unwrap_or_default();
    let wp_field = report
        .map(|r| format!(",\"wordpress\":{}", r.to_json()))
        .unwrap_or_default();
    let cloud_field = cloud_report
        .map(|r| format!(",\"cloud\":{}", cloud::report_json(r)))
        .unwrap_or_default();
    format!(
        "{{\"ok\":true,\"slug\":\"{}\",\"version\":{},\"published_at\":{},\"url\":\"/b/{}/\",\"files\":[{}]{}{}{}}}",
        http::json_escape(&bundle.slug),
        bundle.version,
        bundle.published_at,
        http::json_escape(&bundle.slug),
        files.join(","),
        key_field,
        wp_field,
        cloud_field
    )
}

// ——— WordPress ———

/// The bundle, when the supplied key opens it — the same check
/// publish and restore run, shared by the WordPress endpoints.
fn key_opens(store: &Store, slug: &str, supplied: Option<&str>) -> Option<Arc<store::Bundle>> {
    let bundle = store.get(slug)?;
    let supplied_hash = sha256_hex(supplied.unwrap_or("").as_bytes());
    if constant_time_eq(supplied_hash.as_bytes(), bundle.key_sha256.as_bytes()) {
        Some(bundle)
    } else {
        None
    }
}

/// The key gate for the WordPress endpoints, answering in the same
/// words as publish: 404 when there is no bundle, 403 when the key
/// does not open it. On success, the bundle.
fn wp_gate(
    stream: &mut TcpStream,
    headers: &HashMap<String, String>,
    store: &Arc<Store>,
    slug: &str,
) -> Option<Arc<store::Bundle>> {
    let key = headers.get("x-edit-key").map(|s| s.as_str());
    match key_opens(store, slug, key) {
        Some(bundle) => Some(bundle),
        None => {
            if store.get(slug).is_none() {
                json_error(stream, headers, 404, "no such bundle — release it first, then connect WordPress");
            } else {
                json_error(stream, headers, 403, "that edit key does not open this slug");
            }
            None
        }
    }
}

/// The settings as the API may show them: everything except the
/// password, which is only ever reported as present.
fn wp_settings_json(wp: &WpStore, slug: &str, settings: &wordpress::WpSettings) -> String {
    let state = wp.load_state(slug);
    let last = state
        .last
        .as_ref()
        .map(|r| r.to_json())
        .unwrap_or_else(|| "null".to_string());
    format!(
        "{{\"ok\":true,\"configured\":true,\"site\":\"{}\",\"user\":\"{}\",\"password_set\":true,\"mapped_posts\":{},\"last_sync\":{}}}",
        http::json_escape(&settings.site),
        http::json_escape(&settings.user),
        state.map.len(),
        last
    )
}

const WP_BODY_CAP: usize = 64 * 1024;

/// Save (or, with an empty site, remove) a bundle's WordPress
/// connection. Body is a small JSON object: site, user, password.
/// An empty password keeps the one already stored.
fn handle_wp_save(
    stream: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    headers: &HashMap<String, String>,
    store: &Arc<Store>,
    wp: &Arc<WpStore>,
) {
    let slug = headers.get("x-slug").cloned().unwrap_or_default();
    if !valid_slug(&slug) {
        return json_error(stream, headers, 400, "invalid slug");
    }
    let body = match http::read_body(reader, headers, WP_BODY_CAP) {
        Some(b) => b,
        None => return json_error(stream, headers, 413, "settings larger than the limit"),
    };
    if wp_gate(stream, headers, store, &slug).is_none() {
        return;
    }
    let text = String::from_utf8_lossy(&body);
    let site_raw = wordpress::json_string_field(&text, "site").unwrap_or_default();
    if site_raw.trim().is_empty() {
        // An empty site address disconnects: the settings and the
        // bookkeeping go. The posts on WordPress are untouched.
        return match wp.clear(&slug) {
            Ok(()) => {
                let body = "{\"ok\":true,\"configured\":false}";
                http::send_response(stream, 200, JSON, body.as_bytes(), CacheClass::NoStore, headers, false)
            }
            Err(e) => {
                eprintln!("markdown-wizard: wordpress disconnect {slug} failed: {e}");
                json_error(stream, headers, 500, "the disconnect could not be stored")
            }
        };
    }
    let site = match wordpress::clean_site_url(&site_raw) {
        Ok(s) => s,
        Err(msg) => return json_error(stream, headers, 422, &msg),
    };
    let user_raw = wordpress::json_string_field(&text, "user").unwrap_or_default();
    let user = match wordpress::clean_user(&user_raw) {
        Ok(u) => u,
        Err(msg) => return json_error(stream, headers, 422, &msg),
    };
    let password_field = wordpress::json_string_field(&text, "password").unwrap_or_default();
    let password = if !password_field.is_empty() {
        if password_field.chars().any(|c| c.is_control()) {
            return json_error(stream, headers, 422, "the application password has unexpected characters in it");
        }
        password_field
    } else {
        match wp.load_settings(&slug) {
            Some(existing) => existing.password,
            None => {
                return json_error(
                    stream,
                    headers,
                    422,
                    "an application password is needed — mint one in your WordPress profile (Users → Profile → Application Passwords) and paste it here",
                )
            }
        }
    };
    let settings = wordpress::WpSettings {
        site,
        user,
        password,
    };
    match wp.save_settings(&slug, &settings) {
        Ok(()) => {
            let body = wp_settings_json(wp, &slug, &settings);
            http::send_response(stream, 200, JSON, body.as_bytes(), CacheClass::NoStore, headers, false)
        }
        Err(e) => {
            eprintln!("markdown-wizard: wordpress settings {slug} failed: {e}");
            json_error(stream, headers, 500, "the settings could not be stored")
        }
    }
}

/// Read a bundle's WordPress connection back — never the password.
fn handle_wp_get(
    stream: &mut TcpStream,
    headers: &HashMap<String, String>,
    store: &Arc<Store>,
    wp: &Arc<WpStore>,
    slug: &str,
    head_only: bool,
) {
    if !valid_slug(slug) {
        return json_error(stream, headers, 400, "invalid slug");
    }
    if wp_gate(stream, headers, store, slug).is_none() {
        return;
    }
    match wp.load_settings(slug) {
        Some(settings) => {
            let body = wp_settings_json(wp, slug, &settings);
            http::send_response(stream, 200, JSON, body.as_bytes(), CacheClass::NoStore, headers, head_only)
        }
        None => {
            let body = "{\"ok\":true,\"configured\":false}";
            http::send_response(stream, 200, JSON, body.as_bytes(), CacheClass::NoStore, headers, head_only)
        }
    }
}

/// Re-push the current published bundle without re-publishing —
/// the retry the publish result offers when a file failed.
fn handle_wp_sync(
    stream: &mut TcpStream,
    headers: &HashMap<String, String>,
    store: &Arc<Store>,
    wp: &Arc<WpStore>,
) {
    let slug = headers.get("x-slug").cloned().unwrap_or_default();
    if !valid_slug(&slug) {
        return json_error(stream, headers, 400, "invalid slug");
    }
    let bundle = match wp_gate(stream, headers, store, &slug) {
        Some(b) => b,
        None => return,
    };
    let settings = match wp.load_settings(&slug) {
        Some(s) => s,
        None => return json_error(stream, headers, 422, "WordPress is not connected for this bundle"),
    };
    let report = wordpress::sync_bundle(wp, &bundle, &settings);
    let body = format!("{{\"ok\":true,\"wordpress\":{}}}", report.to_json());
    http::send_response(stream, 200, JSON, body.as_bytes(), CacheClass::NoStore, headers, false)
}

// ——— Cloud sync (S3 / Azure / GCS) ———

/// The cloud gate, in the WordPress gate's shape: the bundle must
/// exist (a connection attaches to a published bundle) and the key
/// must open it.
fn cloud_gate(
    stream: &mut TcpStream,
    headers: &HashMap<String, String>,
    store: &Arc<Store>,
    slug: &str,
) -> Option<Arc<store::Bundle>> {
    if store.get(slug).is_none() {
        json_error(stream, headers, 404, "no such bundle — release it first, then connect cloud storage");
        return None;
    }
    let key = headers.get("x-edit-key").map(|s| s.as_str());
    match key_opens(store, slug, key) {
        Some(b) => Some(b),
        None => {
            json_error(stream, headers, 403, "that edit key does not open this slug");
            None
        }
    }
}

/// Save (or replace) the cloud connection. The body is the
/// settings' own lines form (`key=value`); credential lines left
/// blank keep what is saved. An empty identity line — the bucket
/// for S3/GCS, the container for Azure — disconnects, the
/// WordPress empty-site rule in this record's own terms.
fn handle_cloud_save(
    stream: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    headers: &HashMap<String, String>,
    store: &Arc<Store>,
    cloudstore: &Arc<CloudStore>,
) {
    let slug = headers.get("x-slug").cloned().unwrap_or_default();
    if !valid_slug(&slug) {
        return json_error(stream, headers, 400, "invalid slug");
    }
    let body = match http::read_body(reader, headers, 16 * 1024) {
        Some(b) => b,
        None => return json_error(stream, headers, 413, "settings larger than the 16 KiB limit"),
    };
    if cloud_gate(stream, headers, store, &slug).is_none() {
        return;
    }
    let text = String::from_utf8_lossy(&body);
    // Disconnect: the active provider's identity line, present
    // and empty. (A save body always carries its provider line;
    // without one, the stored provider decides.)
    let existing = cloudstore.load_settings(&slug);
    let mut provider = existing.as_ref().map(|s| s.provider).unwrap_or_default();
    let mut identity_empty = false;
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "provider" => {
                if let Some(p) = cloud::Provider::parse(v) {
                    provider = p;
                }
            }
            "bucket" if v.trim().is_empty() && provider != cloud::Provider::Azure => {
                identity_empty = true;
            }
            "container" if v.trim().is_empty() && provider == cloud::Provider::Azure => {
                identity_empty = true;
            }
            _ => {}
        }
    }
    if identity_empty {
        return match cloudstore.clear(&slug) {
            Ok(()) => http::send_response(
                stream,
                200,
                JSON,
                b"{\"ok\":true,\"configured\":false}",
                CacheClass::NoStore,
                headers,
                false,
            ),
            Err(e) => json_error(stream, headers, 500, &e),
        };
    }
    let settings = match cloud::merge_settings(existing.as_ref(), &text) {
        Ok(s) => s,
        Err(e) => return json_error(stream, headers, 422, &e),
    };
    if let Err(e) = cloudstore.save_settings(&slug, &settings) {
        return json_error(stream, headers, 500, &e);
    }
    let body = cloud::settings_json(Some(&settings), &cloudstore.load_state(&slug));
    http::send_response(stream, 200, JSON, body.as_bytes(), CacheClass::NoStore, headers, false)
}

fn handle_cloud_get(
    stream: &mut TcpStream,
    headers: &HashMap<String, String>,
    store: &Arc<Store>,
    cloudstore: &Arc<CloudStore>,
    slug: &str,
    head_only: bool,
) {
    if !valid_slug(slug) {
        return json_error(stream, headers, 400, "invalid slug");
    }
    if cloud_gate(stream, headers, store, slug).is_none() {
        return;
    }
    let body = cloud::settings_json(
        cloudstore.load_settings(slug).as_ref(),
        &cloudstore.load_state(slug),
    );
    http::send_response(stream, 200, JSON, body.as_bytes(), CacheClass::NoStore, headers, head_only)
}

/// The retry: sync the stored bundle to the stored connection,
/// without re-publishing.
fn handle_cloud_sync(
    stream: &mut TcpStream,
    headers: &HashMap<String, String>,
    store: &Arc<Store>,
    cloudstore: &Arc<CloudStore>,
) {
    let slug = headers.get("x-slug").cloned().unwrap_or_default();
    if !valid_slug(&slug) {
        return json_error(stream, headers, 400, "invalid slug");
    }
    let bundle = match cloud_gate(stream, headers, store, &slug) {
        Some(b) => b,
        None => return,
    };
    let settings = match cloudstore.load_settings(&slug) {
        Some(s) => s,
        None => {
            return json_error(
                stream,
                headers,
                422,
                "cloud storage is not connected for this bundle",
            )
        }
    };
    match cloud::sync_bundle(cloudstore, &bundle, &settings) {
        Ok(report) => {
            let body = format!("{{\"ok\":true,\"cloud\":{}}}", cloud::report_json(&report));
            http::send_response(stream, 200, JSON, body.as_bytes(), CacheClass::NoStore, headers, false)
        }
        Err(e) => json_error(stream, headers, 422, &e),
    }
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
    // The static site, to take away and host anywhere: the whole
    // bundle pre-rendered — pages, PDFs, sources, feed — as one zip
    // (built at publish time; see outputs.rs).
    if sub == "site.zip" {
        if let Some(bytes) = store.site_file(slug, "deploy.zip") {
            let extra = format!(
                "Content-Disposition: attachment; filename=\"{slug}-site.zip\"\r\n"
            );
            http::send_response_extra(
                stream,
                200,
                "application/zip",
                &bytes,
                CacheClass::Shell,
                headers,
                head_only,
                &extra,
            );
            return;
        }
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
    // The pre-rendered page: the reading view as a static file,
    // rendered at publish time (see outputs.rs). A carried file at
    // this exact address was already served above — it wins, here
    // as everywhere.
    if sub.ends_with(".html") {
        let stem = &sub[..sub.len() - ".html".len()];
        if let Some(f) = markdown_file_for_stem(&bundle, stem) {
            if let Some(bytes) = store.site_file(slug, &sub) {
                http::send_response(stream, 200, HTML, &bytes, CacheClass::Shell, headers, head_only);
                return;
            }
            // The cached page is missing (its generation failed,
            // and was logged): the reading view, rendered now, is
            // the same page — serve it rather than nothing.
            let page = render::reading_page(&bundle, f, &base);
            http::send_response(stream, 200, HTML, page.as_bytes(), CacheClass::Shell, headers, head_only);
            return;
        }
    }
    // The printed page: the same rendering as a PDF, printed at
    // publish time and cached with the snapshot. When this server
    // has no Chromium to print with, the address answers with a
    // page that says so.
    if sub.ends_with(".pdf") {
        let stem = &sub[..sub.len() - ".pdf".len()];
        if let Some(f) = markdown_file_for_stem(&bundle, stem) {
            if let Some(bytes) = store.site_file(slug, &sub) {
                http::send_response(stream, 200, "application/pdf", &bytes, CacheClass::Shell, headers, head_only);
                return;
            }
            let page = render::pdf_unavailable_page(&bundle, f);
            http::send_response(stream, 404, HTML, page.as_bytes(), CacheClass::NoStore, headers, head_only);
            return;
        }
    }
    // The site's favicon, for the relative favicon links the
    // pre-rendered pages carry. (A bundle's own favicon.svg was
    // served above, as a carried file — it keeps its address.)
    if sub == "favicon.svg" {
        http::send_response(stream, 200, "image/svg+xml", FAVICON_SVG.as_bytes(), CacheClass::Static, headers, head_only);
        return;
    }
    // The reading view: the same address without the extension.
    for ext in [".md", ".markdown"] {
        let candidate = format!("{sub}{ext}");
        if let Some(f) = bundle.parsed.file(&candidate) {
            let page = render::reading_page(&bundle, f, &base);
            http::send_response(stream, 200, HTML, page.as_bytes(), CacheClass::Shell, headers, head_only);
            return;
        }
    }
    let page = render::not_found_page(path);
    http::send_response(stream, 404, HTML, page.as_bytes(), CacheClass::NoStore, headers, head_only);
}

/// The markdown file a stem names (`essays/one` → `essays/one.md`).
fn markdown_file_for_stem<'a>(bundle: &'a store::Bundle, stem: &str) -> Option<&'a bundle::BundleFile> {
    for ext in [".md", ".markdown"] {
        if let Some(f) = bundle.parsed.file(&format!("{stem}{ext}")) {
            return Some(f);
        }
    }
    None
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
