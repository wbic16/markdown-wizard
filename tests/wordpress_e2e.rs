//! WordPress sync, end to end: the real server against a stub
//! WordPress — a tiny in-process double implementing the two REST
//! routes the integration uses, on 127.0.0.1, no network, no real
//! WordPress. It records what it was sent (including the
//! Authorization header) and can be told to fail one named post
//! exactly once, so the failure and retry legs are deterministic.
//!
//! The guarantees under test are the design's: publish creates the
//! posts and stores their IDs; re-publish updates the same IDs in
//! place; a WordPress failure leaves the local publish standing and
//! is recorded per file; the retry re-pushes and lands; a file that
//! leaves the bundle is never deleted on WordPress, only unmapped;
//! and the application password appears in no response, no download,
//! and no page.
//!
//! The second test drives the same cycle through MCP: an agent's
//! publish_bundle pushes through the same hook a web publish uses —
//! posts created on the first connected publish, updated in place
//! on the next, the report riding the tool result.

use serde_json::{json, Value};

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ——— the markdown-wizard server (the http_e2e.rs pattern) ———

struct Server {
    child: Child,
    port: u16,
    _dir: tempfile::TempDir,
}

fn start_server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_markdown-wizard"))
        .args(["--dir", dir.path().to_str().unwrap(), "--port", "0", "--host", "127.0.0.1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while stdout.read(&mut byte).unwrap_or(0) == 1 {
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    let text = String::from_utf8(line).unwrap();
    let port: u16 = text
        .split("127.0.0.1:")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no port in banner: {text}"));
    Server { child, port, _dir: dir }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Response {
    status: u16,
    body: Vec<u8>,
}

fn request(port: u16, method: &str, path: &str, headers: &[(&str, String)], body: &[u8]) -> Response {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() || method == "POST" {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response has a header block");
    let head_text = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status: u16 = head_text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Response { status, body: raw[split + 4..].to_vec() }
}

fn get(port: u16, path: &str, headers: &[(&str, String)]) -> Response {
    request(port, "GET", path, headers, &[])
}

fn publish(port: u16, slug: &str, zip: &[u8], key: Option<&str>) -> Response {
    let mut headers = vec![("X-Slug", slug.to_string())];
    if let Some(k) = key {
        headers.push(("X-Edit-Key", k.to_string()));
    }
    request(port, "POST", "/api/publish", &headers, zip)
}

fn json_str(body: &[u8], field: &str) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let pat = format!("\"{field}\":\"");
    let start = text.find(&pat)? + pat.len();
    let end = text[start..].find('"')? + start;
    Some(text[start..end].to_string())
}

/// The recorded outcome for one file in a sync report.
fn outcome_of(body: &[u8], path: &str) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let pat = format!("\"path\":\"{path}\"");
    let at = text.find(&pat)?;
    json_str(text[at..].as_bytes(), "outcome")
}

fn body_text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

// ——— the stub WordPress ———

#[derive(Default)]
struct Stub {
    /// post ID -> the raw JSON body last sent for it
    posts: Mutex<HashMap<u64, String>>,
    next_id: Mutex<u64>,
    /// (method, path, authorization, body) for every request
    log: Mutex<Vec<(String, String, String, String)>>,
    /// WP slugs whose next create/update answers 500, once
    fail_once: Mutex<HashSet<String>>,
}

struct StubServer {
    port: u16,
    stub: Arc<Stub>,
}

fn start_stub() -> StubServer {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = Arc::new(Stub::default());
    let thread_stub = stub.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(stream) = stream {
                let stub = thread_stub.clone();
                std::thread::spawn(move || stub_handle(stream, stub));
            }
        }
    });
    StubServer { port, stub }
}

fn stub_handle(mut stream: TcpStream, stub: Arc<Stub>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let cloned = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(cloned);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut parts = request_line.trim().split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut content_length = 0usize;
    let mut authorization = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_lowercase();
            if k == "content-length" {
                content_length = v.trim().parse().unwrap_or(0);
            } else if k == "authorization" {
                authorization = v.trim().to_string();
            }
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 && reader.read_exact(&mut body).is_err() {
        return;
    }
    let body_text = String::from_utf8_lossy(&body).into_owned();
    stub.log.lock().unwrap().push((
        method.clone(),
        path.clone(),
        authorization,
        body_text.clone(),
    ));

    let (status, payload) = stub_route(&stub, &method, &path, &body_text);
    let response = format!(
        "HTTP/1.1 {status} Stub\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn stub_json_str(body: &str, field: &str) -> Option<String> {
    let pat = format!("\"{field}\":\"");
    let start = body.find(&pat)? + pat.len();
    let end = body[start..].find('"')? + start;
    Some(body[start..end].to_string())
}

fn stub_fail_armed(stub: &Stub, body: &str) -> bool {
    if let Some(slug) = stub_json_str(body, "slug") {
        let mut armed = stub.fail_once.lock().unwrap();
        if armed.remove(&slug) {
            return true;
        }
    }
    false
}

fn stub_route(stub: &Stub, method: &str, path: &str, body: &str) -> (u16, String) {
    let fail_body =
        "{\"code\":\"stub_failure\",\"message\":\"the stub was told to fail this one\"}".to_string();
    if method == "POST" && path == "/wp-json/wp/v2/posts" {
        if stub_fail_armed(stub, body) {
            return (500, fail_body);
        }
        let mut next = stub.next_id.lock().unwrap();
        *next += 1;
        let id = *next;
        stub.posts.lock().unwrap().insert(id, body.to_string());
        return (
            201,
            format!(
                "{{\"id\":{id},\"link\":\"http://stub.invalid/?p={id}\",\"slug\":\"{}\"}}",
                stub_json_str(body, "slug").unwrap_or_default()
            ),
        );
    }
    if method == "POST" {
        if let Some(id_part) = path.strip_prefix("/wp-json/wp/v2/posts/") {
            if let Ok(id) = id_part.parse::<u64>() {
                if !stub.posts.lock().unwrap().contains_key(&id) {
                    return (
                        404,
                        "{\"code\":\"rest_post_invalid_id\",\"message\":\"Invalid post ID.\"}"
                            .to_string(),
                    );
                }
                if stub_fail_armed(stub, body) {
                    return (500, fail_body);
                }
                stub.posts.lock().unwrap().insert(id, body.to_string());
                return (
                    200,
                    format!("{{\"id\":{id},\"link\":\"http://stub.invalid/?p={id}\"}}"),
                );
            }
        }
    }
    (404, "{\"code\":\"rest_no_route\",\"message\":\"no route\"}".to_string())
}

// ——— zip building (the http_e2e.rs pattern) ———

fn make_zip(files: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, text) in files {
        let offset = out.len() as u32;
        let data = text.as_bytes();
        let crc = crc32(data);
        let mut local = Vec::new();
        local.extend_from_slice(&0x04034b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes());
        local.extend_from_slice(&0x0800u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0x21u16.to_le_bytes());
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&(data.len() as u32).to_le_bytes());
        local.extend_from_slice(&(data.len() as u32).to_le_bytes());
        local.extend_from_slice(&(name.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&local);
        out.extend_from_slice(data);
        let mut cen = Vec::new();
        cen.extend_from_slice(&0x02014b50u32.to_le_bytes());
        cen.extend_from_slice(&20u16.to_le_bytes());
        cen.extend_from_slice(&20u16.to_le_bytes());
        cen.extend_from_slice(&0x0800u16.to_le_bytes());
        cen.extend_from_slice(&0u16.to_le_bytes());
        cen.extend_from_slice(&0u16.to_le_bytes());
        cen.extend_from_slice(&0x21u16.to_le_bytes());
        cen.extend_from_slice(&crc.to_le_bytes());
        cen.extend_from_slice(&(data.len() as u32).to_le_bytes());
        cen.extend_from_slice(&(data.len() as u32).to_le_bytes());
        cen.extend_from_slice(&(name.len() as u16).to_le_bytes());
        cen.extend_from_slice(&[0u8; 12]);
        cen.extend_from_slice(&offset.to_le_bytes());
        cen.extend_from_slice(name.as_bytes());
        central.extend_from_slice(&cen);
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    let mut end = Vec::new();
    end.extend_from_slice(&0x06054b50u32.to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes());
    end.extend_from_slice(&(files.len() as u16).to_le_bytes());
    end.extend_from_slice(&(files.len() as u16).to_le_bytes());
    end.extend_from_slice(&(central.len() as u32).to_le_bytes());
    end.extend_from_slice(&cd_offset.to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&end);
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (n, slot) in table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb88320 ^ (c >> 1) } else { c >> 1 };
        }
        *slot = c;
    }
    let mut c = 0xffffffffu32;
    for &b in bytes {
        c = table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xffffffff
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0] as u32,
            *chunk.get(1).unwrap_or(&0) as u32,
            *chunk.get(2).unwrap_or(&0) as u32,
        ];
        let n = (b[0] << 16) | (b[1] << 8) | b[2];
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

// ——— the cycle ———

const PASSWORD: &str = "test-app-password";
const INDEX: &str = "Dear reader,\n\nThis is where the WordPress bundle begins.\n";
const ABOUT: &str = "The short version of the WordPress bundle.\n";

fn save_settings(port: u16, slug: &str, key: &str, site: &str) -> Response {
    let body = format!(
        "{{\"site\":\"{site}\",\"user\":\"wp-user\",\"password\":\"{PASSWORD}\"}}"
    );
    request(
        port,
        "POST",
        "/api/wordpress",
        &[
            ("X-Slug", slug.to_string()),
            ("X-Edit-Key", key.to_string()),
            ("Content-Type", "application/json".to_string()),
        ],
        body.as_bytes(),
    )
}

#[test]
fn wordpress_sync_cycle() {
    let srv = start_server();
    let stub = start_stub();
    let site = format!("http://127.0.0.1:{}", stub.port);

    // Publish v1 with no WordPress connected: no push, no field.
    let zip1 = make_zip(&[
        ("index.md", INDEX),
        ("about.md", ABOUT),
        ("essays/one.md", "# One\n\nFirst version.\n"),
        ("vault/secret.md", "# Hidden\n\nNever sent.\n"),
    ]);
    let r = publish(srv.port, "wp-bundle", &zip1, None);
    assert_eq!(r.status, 201, "{}", body_text(&r.body));
    let key = json_str(&r.body, "edit_key").expect("first publish returns the key");
    assert!(!body_text(&r.body).contains("wordpress"), "no sync before a connection exists");
    assert!(stub.stub.log.lock().unwrap().is_empty(), "the stub heard nothing");

    // The settings endpoints are key-gated like publish itself.
    let bad = save_settings(srv.port, "wp-bundle", "not-the-key", &site);
    assert_eq!(bad.status, 403);
    let no_key = get(srv.port, "/api/wordpress/wp-bundle", &[]);
    assert_eq!(no_key.status, 403);

    // Connect. The password is acknowledged as set, never shown.
    let saved = save_settings(srv.port, "wp-bundle", &key, &site);
    assert_eq!(saved.status, 200, "{}", body_text(&saved.body));
    assert!(body_text(&saved.body).contains("\"password_set\":true"));
    assert!(!body_text(&saved.body).contains(PASSWORD), "the password is never echoed");
    let read_back = get(
        srv.port,
        "/api/wordpress/wp-bundle",
        &[("X-Edit-Key", key.clone())],
    );
    assert_eq!(read_back.status, 200);
    assert!(body_text(&read_back.body).contains("\"configured\":true"));
    assert!(!body_text(&read_back.body).contains(PASSWORD));

    // Re-publish: every publishable file becomes a post. The vault
    // file is not one of them, here as everywhere.
    let zip2 = make_zip(&[
        ("index.md", INDEX),
        ("about.md", ABOUT),
        ("essays/one.md", "# One\n\nSecond version.\n"),
        ("vault/secret.md", "# Hidden\n\nNever sent.\n"),
    ]);
    let r2 = publish(srv.port, "wp-bundle", &zip2, Some(&key));
    assert_eq!(r2.status, 200, "{}", body_text(&r2.body));
    let text2 = body_text(&r2.body);
    for path in ["index.md", "about.md", "essays/one.md"] {
        assert_eq!(outcome_of(&r2.body, path).as_deref(), Some("created"), "{path}: {text2}");
    }
    assert!(
        !text2.contains("\"path\":\"vault/secret.md\""),
        "the vault is never synced: {text2}"
    );
    {
        let posts = stub.stub.posts.lock().unwrap();
        assert_eq!(posts.len(), 3, "three posts created");
        let one = posts.values().find(|b| b.contains("essays-one")).expect("the essay was posted");
        assert!(one.contains("Second version."), "the rendered HTML was pushed: {one}");
        assert!(one.contains("\\\"status\\\":\\\"publish\\\"") || one.contains("\"status\":\"publish\""), "{one}");
    }
    // The stub saw the application password as a Basic credential —
    // and only there.
    let expected_auth = format!("Basic {}", base64_encode(format!("wp-user:{PASSWORD}").as_bytes()));
    {
        let log = stub.stub.log.lock().unwrap();
        assert!(log.iter().all(|(_, _, auth, _)| auth == &expected_auth), "auth header on every call");
        assert!(log.iter().all(|(_, _, _, body)| !body.contains("secret")), "no vault content sent");
    }

    // Re-publish again: the same posts, updated in place.
    let zip3 = make_zip(&[
        ("index.md", INDEX),
        ("about.md", "The short version, revised.\n"),
        ("essays/one.md", "# One\n\nSecond version.\n"),
    ]);
    let r3 = publish(srv.port, "wp-bundle", &zip3, Some(&key));
    assert_eq!(r3.status, 200);
    for path in ["index.md", "about.md", "essays/one.md"] {
        assert_eq!(outcome_of(&r3.body, path).as_deref(), Some("updated"), "{path}");
    }
    {
        let posts = stub.stub.posts.lock().unwrap();
        assert_eq!(posts.len(), 3, "still three posts — updates, not duplicates");
        assert!(
            posts.values().any(|b| b.contains("revised")),
            "the update carried the new text"
        );
    }

    // A WordPress failure on one file: the local publish stands at
    // its new version, the failure is recorded with the site's own
    // words, and the other file still lands.
    stub.stub.fail_once.lock().unwrap().insert("essays-one".to_string());
    let zip4 = make_zip(&[
        ("index.md", INDEX),
        ("about.md", "The short version, revised again.\n"),
        ("essays/one.md", "# One\n\nThird version.\n"),
    ]);
    let r4 = publish(srv.port, "wp-bundle", &zip4, Some(&key));
    assert_eq!(r4.status, 200, "a WordPress failure never fails the publish");
    let served = get(srv.port, "/b/wp-bundle/essays/one.md", &[]);
    assert_eq!(served.body, b"# One\n\nThird version.\n", "the local bundle moved on");
    assert_eq!(outcome_of(&r4.body, "essays/one.md").as_deref(), Some("failed"));
    assert!(
        body_text(&r4.body).contains("the stub was told to fail this one"),
        "the site's error is recorded: {}",
        body_text(&r4.body)
    );
    assert_eq!(outcome_of(&r4.body, "about.md").as_deref(), Some("updated"));

    // The retry re-pushes without re-publishing, and lands — into
    // the same post, because the mapping held through the failure.
    let retry = request(
        srv.port,
        "POST",
        "/api/wordpress/sync",
        &[("X-Slug", "wp-bundle".to_string()), ("X-Edit-Key", key.clone())],
        &[],
    );
    assert_eq!(retry.status, 200, "{}", body_text(&retry.body));
    assert_eq!(outcome_of(&retry.body, "essays/one.md").as_deref(), Some("updated"));
    {
        let posts = stub.stub.posts.lock().unwrap();
        assert_eq!(posts.len(), 3, "the retry updated in place");
        assert!(posts.values().any(|b| b.contains("Third version.")));
    }

    // A file leaves the bundle: no DELETE is ever sent, the post
    // stays on WordPress, and the report names the drop.
    let zip5 = make_zip(&[("index.md", INDEX), ("about.md", ABOUT)]);
    let r5 = publish(srv.port, "wp-bundle", &zip5, Some(&key));
    assert_eq!(r5.status, 200);
    assert!(
        body_text(&r5.body).contains("\"dropped\":[\"essays/one.md\"]"),
        "the drop is reported: {}",
        body_text(&r5.body)
    );
    {
        let log = stub.stub.log.lock().unwrap();
        assert!(log.iter().all(|(m, _, _, _)| m == "POST"), "nothing but POSTs — no delete");
        assert_eq!(stub.stub.posts.lock().unwrap().len(), 3, "the post itself is untouched");
    }

    // The password is in none of the public surfaces.
    let dl = get(srv.port, "/b/wp-bundle.zip", &[]);
    assert!(!body_text(&dl.body).contains(PASSWORD), "not in the download");
    let index = get(srv.port, "/b/wp-bundle/", &[]);
    assert!(!body_text(&index.body).contains(PASSWORD), "not in the index page");

    // WordPress.com and cleartext-remote addresses are refused at
    // save time, in plain words.
    let wpcom = save_settings(srv.port, "wp-bundle", &key, "https://someone.wordpress.com");
    assert_eq!(wpcom.status, 422);
    assert!(body_text(&wpcom.body).contains("isn't supported yet"), "{}", body_text(&wpcom.body));
    let plain = save_settings(srv.port, "wp-bundle", &key, "http://example.com");
    assert_eq!(plain.status, 422);
    assert!(body_text(&plain.body).contains("https"), "{}", body_text(&plain.body));
}

// ——— the MCP leg: an agent's publish pushes too ———

/// One MCP tools/call against the real server; returns the tool
/// result's payload (its text content, parsed back from JSON).
fn mcp_call(port: u16, id: u64, name: &str, args: Value) -> Value {
    let req = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}});
    let body = serde_json::to_vec(&req).unwrap();
    let r = request(
        port,
        "POST",
        "/mcp",
        &[("Content-Type", "application/json".to_string())],
        &body,
    );
    assert_eq!(r.status, 200, "tools/call {name}: {}", body_text(&r.body));
    let resp: Value = serde_json::from_slice(&r.body).unwrap();
    let result = &resp["result"];
    assert_eq!(result["isError"], json!(false), "tool {name} errored: {result}");
    let text = result["content"][0]["text"].as_str().expect("text content");
    serde_json::from_str(text).unwrap_or_else(|_| panic!("tool text is not JSON: {text}"))
}

/// The recorded outcome for one file in an MCP publish's
/// wordpress report.
fn mcp_outcome(payload: &Value, path: &str) -> Option<String> {
    payload["wordpress"]["results"]
        .as_array()?
        .iter()
        .find(|r| r["path"] == json!(path))
        .and_then(|r| r["outcome"].as_str().map(str::to_string))
}

#[test]
fn mcp_publish_pushes_to_wordpress() {
    let srv = start_server();
    let stub = start_stub();
    let site = format!("http://127.0.0.1:{}", stub.port);

    // The agent writes a bundle into a fresh slug; the first write
    // mints the key, as it does for a human's first publish.
    let first = mcp_call(
        srv.port,
        1,
        "write_file",
        json!({"slug": "mcp-bundle", "path": "index.md", "content": "# Agent Index\n\nFirst agent version.\n"}),
    );
    let key = first["edit_key"].as_str().expect("the first write mints the key").to_string();
    mcp_call(
        srv.port,
        2,
        "write_file",
        json!({"slug": "mcp-bundle", "path": "about.md", "content": "The agent's short version.\n", "edit_key": key}),
    );
    mcp_call(
        srv.port,
        3,
        "write_file",
        json!({"slug": "mcp-bundle", "path": "vault/secret.md", "content": "# Hidden\n\nNever sent.\n", "edit_key": key}),
    );

    // Publish with no WordPress connected: no push, no field —
    // exactly the web path's behavior before a connection exists.
    let pub1 = mcp_call(
        srv.port,
        4,
        "publish_bundle",
        json!({"slug": "mcp-bundle", "changed": "Agent release one", "edit_key": key}),
    );
    assert_eq!(pub1["version"], json!(1), "{pub1}");
    assert!(pub1.get("wordpress").is_none(), "no sync before a connection exists: {pub1}");
    assert!(stub.stub.log.lock().unwrap().is_empty(), "the stub heard nothing");

    // Connect the stub site (the settings endpoint is the web one;
    // the key the agent minted opens it).
    let saved = save_settings(srv.port, "mcp-bundle", &key, &site);
    assert_eq!(saved.status, 200, "{}", body_text(&saved.body));

    // The agent edits and re-publishes over MCP: the push rides
    // this publish too. Every publishable file becomes a post; the
    // vault file is not one of them.
    mcp_call(
        srv.port,
        5,
        "write_file",
        json!({"slug": "mcp-bundle", "path": "index.md", "content": "# Agent Index\n\nSecond agent version.\n", "edit_key": key}),
    );
    let pub2 = mcp_call(
        srv.port,
        6,
        "publish_bundle",
        json!({"slug": "mcp-bundle", "changed": "Agent release two", "edit_key": key}),
    );
    assert_eq!(pub2["version"], json!(2), "{pub2}");
    assert_eq!(pub2["wordpress"]["site"], json!(site), "the report names the site: {pub2}");
    for path in ["index.md", "about.md"] {
        assert_eq!(mcp_outcome(&pub2, path).as_deref(), Some("created"), "{path}: {pub2}");
    }
    assert!(
        mcp_outcome(&pub2, "vault/secret.md").is_none(),
        "the vault is never synced: {pub2}"
    );
    {
        let posts = stub.stub.posts.lock().unwrap();
        assert_eq!(posts.len(), 2, "two posts created by the MCP publish");
        assert!(
            posts.values().any(|b| b.contains("Second agent version.")),
            "the rendered HTML of the agent's edit was pushed"
        );
    }

    // The report is persisted with the bundle, the same as a web
    // publish's: the settings read-back carries it as last_sync.
    let read_back = get(
        srv.port,
        "/api/wordpress/mcp-bundle",
        &[("X-Edit-Key", key.clone())],
    );
    assert_eq!(read_back.status, 200);
    let back = body_text(&read_back.body);
    assert!(back.contains("\"mapped_posts\":2"), "the mapping persisted: {back}");
    assert!(back.contains("\"last_sync\":{"), "the report persisted: {back}");
    assert!(back.contains("\"outcome\":\"created\""), "with its outcomes: {back}");

    // A third MCP publish updates the same posts in place.
    mcp_call(
        srv.port,
        7,
        "write_file",
        json!({"slug": "mcp-bundle", "path": "about.md", "content": "The agent's short version, revised.\n", "edit_key": key}),
    );
    let pub3 = mcp_call(
        srv.port,
        8,
        "publish_bundle",
        json!({"slug": "mcp-bundle", "changed": "Agent release three", "edit_key": key}),
    );
    assert_eq!(pub3["version"], json!(3), "{pub3}");
    for path in ["index.md", "about.md"] {
        assert_eq!(mcp_outcome(&pub3, path).as_deref(), Some("updated"), "{path}: {pub3}");
    }
    {
        let posts = stub.stub.posts.lock().unwrap();
        assert_eq!(posts.len(), 2, "still two posts — updates, not duplicates");
        assert!(
            posts.values().any(|b| b.contains("revised")),
            "the update carried the agent's new text"
        );
    }
}
