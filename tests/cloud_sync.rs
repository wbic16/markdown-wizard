//! Cloud sync, end to end against a stub: a local test double
//! that records every request and answers like the provider it is
//! standing in for (200/201 + an ETag, or a 500 with that
//! provider's error document when armed to fail). No real cloud,
//! no real credentials — the endpoint setting points the sync at
//! the stub, which is also what a compatible store (or Azurite)
//! looks like from the client's side.
//!
//! The guarantees under test, per provider: a sync uploads the
//! deployable tree (the outputs build's on-disk site tree, minus
//! its own deploy.zip) with the right content types and a
//! correctly formed authorization header (SigV4 for S3 and GCS —
//! GCS scoped `auto` — Shared Key for Azure); an unchanged file is
//! not re-uploaded; a stub failure is recorded per file and never
//! touches the local publish; the retry re-syncs without
//! re-publishing; and no credential appears in any API response.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::Digest;

// ─────────────────────── the server under test ───────────────────────

struct Server {
    child: Child,
    port: u16,
    dir_path: PathBuf,
    _dir: tempfile::TempDir,
}

fn start_server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
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
    Server { child, port, dir_path, _dir: dir }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The expected payload: every file of the bundle's on-disk site
/// tree except the tree's own deploy.zip, sorted.
fn expected_tree(srv: &Server, slug: &str) -> Vec<String> {
    let site = srv.dir_path.join(slug).join("site");
    let mut out = Vec::new();
    fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                if rel != "deploy.zip" {
                    out.push(rel);
                }
            }
        }
    }
    walk(&site, &site, &mut out);
    out.sort();
    out
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

fn get(port: u16, path: &str) -> Response {
    request(port, "GET", path, &[], &[])
}

fn publish(port: u16, slug: &str, zip: &[u8], key: Option<&str>) -> Response {
    let mut headers = vec![("X-Slug", slug.to_string())];
    if let Some(k) = key {
        headers.push(("X-Edit-Key", k.to_string()));
    }
    request(port, "POST", "/api/publish", &headers, zip)
}

fn save_cloud(port: u16, slug: &str, key: &str, body: &str) -> Response {
    request(
        port,
        "POST",
        "/api/cloud",
        &[
            ("X-Slug", slug.to_string()),
            ("X-Edit-Key", key.to_string()),
            ("Content-Type", "text/plain".to_string()),
        ],
        body.as_bytes(),
    )
}

fn sync_cloud(port: u16, slug: &str, key: &str) -> Response {
    request(
        port,
        "POST",
        "/api/cloud/sync",
        &[("X-Slug", slug.to_string()), ("X-Edit-Key", key.to_string())],
        &[],
    )
}

fn get_cloud(port: u16, slug: &str, key: &str) -> Response {
    request(port, "GET", &format!("/api/cloud/{slug}"), &[("X-Edit-Key", key.to_string())], &[])
}

fn json_str(body: &[u8], field: &str) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let pat = format!("\"{field}\":\"");
    let start = text.find(&pat)? + pat.len();
    let end = text[start..].find('"')? + start;
    Some(text[start..end].to_string())
}

fn json_num(body: &[u8], field: &str) -> Option<u64> {
    let text = String::from_utf8_lossy(body);
    let pat = format!("\"{field}\":");
    let start = text.find(&pat)? + pat.len();
    let num: String = text[start..].chars().take_while(|c| c.is_ascii_digit()).collect();
    num.parse().ok()
}

fn body_text(r: &Response) -> String {
    String::from_utf8_lossy(&r.body).into_owned()
}

// ─────────────────────── zips, as the house builds them ───────────────────────

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

fn fixture_zip(index_line: &str) -> Vec<u8> {
    make_zip(&[
        ("index.md", index_line),
        ("about.md", "# About\n"),
        ("essays/one.md", "# One\n\nFirst version.\n"),
        ("vault/secret.md", "# Secret\n"),
    ])
}

const ZIP_V1: &str = "# Site\n\nThe front door.\n";

// ─────────────────────── the stub cloud ───────────────────────

#[derive(Clone)]
struct Recorded {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

struct StubCloud {
    port: u16,
    requests: Arc<Mutex<Vec<Recorded>>>,
    fail_all: Arc<AtomicBool>,
}

impl StubCloud {
    fn start() -> StubCloud {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
        let fail_all = Arc::new(AtomicBool::new(false));
        let (rq, fl) = (requests.clone(), fail_all.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                let mut reader = BufReader::new(match stream.try_clone() {
                    Ok(s) => s,
                    Err(_) => continue,
                });
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                    continue;
                }
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let mut headers = Vec::new();
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let line = line.trim();
                    if line.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        if k.trim().eq_ignore_ascii_case("content-length") {
                            len = v.trim().parse().unwrap_or(0);
                        }
                        headers.push((k.trim().to_string(), v.trim().to_string()));
                    }
                }
                let mut body = vec![0u8; len];
                if len > 0 {
                    let _ = reader.read_exact(&mut body);
                }
                let is_azure = headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("x-ms-version"));
                rq.lock().unwrap().push(Recorded { method, path, headers, body });
                let response = if fl.load(Ordering::SeqCst) {
                    let xml = if is_azure {
                        "<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>StubBroke</Code><Message>azure stub boom</Message></Error>"
                    } else {
                        "<Error><Code>InternalError</Code><Message>stub boom</Message></Error>"
                    };
                    format!(
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        xml.len(),
                        xml
                    )
                } else if is_azure {
                    "HTTP/1.1 201 Created\r\nETag: \"stub-etag\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else {
                    "HTTP/1.1 200 OK\r\nETag: \"stub-etag\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                };
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        StubCloud { port, requests, fail_all }
    }

    fn puts(&self) -> Vec<Recorded> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == "PUT")
            .cloned()
            .collect()
    }

    fn put_paths(&self) -> Vec<String> {
        self.puts().iter().map(|p| p.path.clone()).collect()
    }
}

const SECRET: &str = "test-secret-0000-not-a-real-credential";

fn publish_v1(port: u16) -> String {
    let r = publish(port, "site", &fixture_zip(ZIP_V1), None);
    assert_eq!(r.status, 201, "first publish: {}", body_text(&r));
    assert!(!body_text(&r).contains("\"cloud\""), "no connection, no report");
    json_str(&r.body, "edit_key").expect("an edit key is issued")
}

fn assert_content_types(puts: &[Recorded]) {
    let by_path = |suffix: &str| puts.iter().find(|p| p.path.ends_with(suffix));
    assert_eq!(
        by_path("index.html").and_then(|p| p.header("content-type")),
        Some("text/html")
    );
    assert_eq!(
        by_path("about.md").and_then(|p| p.header("content-type")),
        Some("text/markdown")
    );
    assert_eq!(
        by_path("feed.xml").and_then(|p| p.header("content-type")),
        Some("application/rss+xml")
    );
    if let Some(pdf) = by_path("about.pdf") {
        assert_eq!(pdf.header("content-type"), Some("application/pdf"));
    }
    // The vault never travels.
    assert!(puts.iter().all(|p| !p.path.contains("vault")), "vault file uploaded");
    assert!(puts.iter().all(|p| !p.path.ends_with("deploy.zip")), "deploy.zip uploaded");
}

// ─────────────────────── S3 ───────────────────────

#[test]
fn s3_sync_against_a_stub() {
    let stub = StubCloud::start();
    let srv = start_server();
    let port = srv.port;
    let key = publish_v1(port);
    let tree = expected_tree(&srv, "site");
    assert!(tree.len() >= 10, "tree too small: {tree:?}");

    let settings = format!(
        "provider=s3\nbucket=my-bucket\nregion=us-east-1\nprefix=site\nendpoint=http://127.0.0.1:{}\naccess_key_id=TESTAKID\nsecret_access_key={SECRET}\n",
        stub.port
    );

    // The gate: a wrong key is refused, an unknown slug is a 404.
    let r = save_cloud(port, "site", "wrong-key", &settings);
    assert_eq!(r.status, 403, "wrong key: {}", body_text(&r));
    let r = save_cloud(port, "ghost", &key, &settings);
    assert_eq!(r.status, 404, "unknown slug: {}", body_text(&r));

    // Save: the answer names the key by its last four and never
    // carries the secret.
    let r = save_cloud(port, "site", &key, &settings);
    assert_eq!(r.status, 200, "save: {}", body_text(&r));
    assert!(body_text(&r).contains("\"provider\":\"s3\""), "{}", body_text(&r));
    assert!(body_text(&r).contains("\"access_key_id_last4\":\"AKID\""), "{}", body_text(&r));
    assert!(!body_text(&r).contains(SECRET), "the secret was echoed");

    // First sync: the whole tree, path-style under the prefix.
    let r = sync_cloud(port, "site", &key);
    assert_eq!(r.status, 200, "first sync: {}", body_text(&r));
    assert_eq!(json_num(&r.body, "uploaded"), Some(tree.len() as u64), "{}", body_text(&r));
    assert_eq!(json_num(&r.body, "failed"), Some(0), "{}", body_text(&r));
    let puts = stub.puts();
    let mut keys: Vec<&str> = puts.iter().map(|p| p.path.as_str()).collect();
    keys.sort();
    let expected: Vec<String> = tree.iter().map(|rel| format!("/my-bucket/site/{rel}")).collect();
    assert_eq!(keys, expected.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    assert_content_types(&puts);
    // The SigV4 shape: the credential scope, and the payload hash
    // the header swears to is the hash of the body that arrived.
    let index_put = puts.iter().find(|p| p.path.ends_with("index.md")).unwrap();
    let auth = index_put.header("authorization").unwrap_or("");
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 Credential=TESTAKID/")
            && auth.contains("/us-east-1/s3/aws4_request")
            && auth.contains("SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date"),
        "{auth}"
    );
    let body_hash = format!("{:x}", sha2::Sha256::digest(&index_put.body));
    assert_eq!(index_put.header("x-amz-content-sha256"), Some(body_hash.as_str()));

    // Second sync: everything skipped, nothing sent.
    let before = stub.puts().len();
    let r = sync_cloud(port, "site", &key);
    assert_eq!(json_num(&r.body, "skipped"), Some(tree.len() as u64), "{}", body_text(&r));
    assert_eq!(stub.puts().len(), before, "an unchanged file was re-uploaded");

    // Publish v2 (one source line changes): the publish response
    // carries the cloud report; the changed source re-uploads and
    // the untouched one does not.
    let zip2 = fixture_zip("# Site\n\nThe front door, revised.\n");
    let r = publish(port, "site", &zip2, Some(&key));
    assert_eq!(r.status, 200, "v2 publish: {}", body_text(&r));
    assert!(body_text(&r).contains("\"cloud\""), "publish carries the report: {}", body_text(&r));
    let paths = stub.put_paths();
    assert!(paths.iter().any(|p| p == "/my-bucket/site/index.md"), "changed source not uploaded: {paths:?}");
    assert!(!paths[before..].iter().any(|p| p == "/my-bucket/site/about.md"), "untouched source re-uploaded");

    // Disconnect: an empty bucket line. The status agrees, and a
    // sync now refuses plainly.
    let r = save_cloud(port, "site", &key, "provider=s3\nbucket=\n");
    assert_eq!(r.status, 200, "disconnect: {}", body_text(&r));
    let r = get_cloud(port, "site", &key);
    assert!(body_text(&r).contains("\"configured\":false"), "{}", body_text(&r));
    let r = sync_cloud(port, "site", &key);
    assert_eq!(r.status, 422, "sync with no connection: {}", body_text(&r));
}

// ─────────────────────── Azure ───────────────────────

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = ((chunk[0] as u32) << 16)
            | ((*chunk.get(1).unwrap_or(&0) as u32) << 8)
            | (*chunk.get(2).unwrap_or(&0) as u32);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[test]
fn azure_sync_against_a_stub() {
    let stub = StubCloud::start();
    let srv = start_server();
    let port = srv.port;
    let key = publish_v1(port);
    let tree = expected_tree(&srv, "site");
    let account_key = b64(&[9u8; 32]);

    let settings = format!(
        "provider=azure\naccount=devstore\ncontainer=site-files\nprefix=web\nendpoint=http://127.0.0.1:{}\naccount_key={account_key}\n",
        stub.port
    );
    let r = save_cloud(port, "site", &key, &settings);
    assert_eq!(r.status, 200, "save: {}", body_text(&r));
    assert!(body_text(&r).contains("\"provider\":\"azure\""), "{}", body_text(&r));
    assert!(body_text(&r).contains("\"has_account_key\":true"), "{}", body_text(&r));
    assert!(!body_text(&r).contains(&account_key), "the account key was echoed");

    // The publish hook carries it: v2's response reports the sync,
    // and every file of the tree lands path-style under the prefix.
    let zip2 = fixture_zip("# Site\n\nThe front door, revised.\n");
    let r = publish(port, "site", &zip2, Some(&key));
    assert_eq!(r.status, 200, "v2 publish: {}", body_text(&r));
    assert!(body_text(&r).contains("\"provider\":\"azure\""), "{}", body_text(&r));
    assert_eq!(json_num(&r.body, "uploaded"), Some(tree.len() as u64), "{}", body_text(&r));
    let puts = stub.puts();
    let mut keys: Vec<&str> = puts.iter().map(|p| p.path.as_str()).collect();
    keys.sort();
    let expected: Vec<String> =
        tree.iter().map(|rel| format!("/devstore/site-files/web/{rel}")).collect();
    assert_eq!(keys, expected.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    assert_content_types(&puts);
    // The Shared Key shape.
    let one = &puts[0];
    let auth = one.header("authorization").unwrap_or("");
    assert!(auth.starts_with("SharedKey devstore:"), "{auth}");
    assert_eq!(one.header("x-ms-blob-type"), Some("BlockBlob"));
    assert_eq!(one.header("x-ms-version"), Some("2020-10-02"));
    assert!(one.header("x-ms-date").unwrap_or("").ends_with(" GMT"));

    // Re-sync: everything skipped.
    let before = stub.puts().len();
    let r = sync_cloud(port, "site", &key);
    assert_eq!(json_num(&r.body, "skipped"), Some(tree.len() as u64), "{}", body_text(&r));
    assert_eq!(stub.puts().len(), before);
}

// ─────────────────────── GCS ───────────────────────

#[test]
fn gcs_sync_against_a_stub() {
    let stub = StubCloud::start();
    let srv = start_server();
    let port = srv.port;
    let key = publish_v1(port);
    let tree = expected_tree(&srv, "site");

    let settings = format!(
        "provider=gcs\nbucket=my-bucket\nprefix=site\nendpoint=http://127.0.0.1:{}\naccess_key_id=TESTHMAC\nsecret_access_key={SECRET}\n",
        stub.port
    );
    let r = save_cloud(port, "site", &key, &settings);
    assert_eq!(r.status, 200, "save: {}", body_text(&r));
    assert!(body_text(&r).contains("\"provider\":\"gcs\""), "{}", body_text(&r));
    assert!(!body_text(&r).contains(SECRET), "the secret was echoed");

    // Arm the stub to fail everything: the sync records a failure
    // per file with the provider's error, and the local publish is
    // untouched.
    stub.fail_all.store(true, Ordering::SeqCst);
    let r = sync_cloud(port, "site", &key);
    assert_eq!(r.status, 200, "failing sync still answers: {}", body_text(&r));
    assert_eq!(json_num(&r.body, "failed"), Some(tree.len() as u64), "{}", body_text(&r));
    assert!(body_text(&r).contains("InternalError"), "{}", body_text(&r));
    let r = get(port, "/b/site/index.md");
    assert_eq!(r.status, 200, "the local publish stands");
    assert!(body_text(&r).contains("The front door."));
    // The recorded state shows the failure on the settings read.
    let r = get_cloud(port, "site", &key);
    assert!(body_text(&r).contains("\"failed\":".to_string().as_str()), "{}", body_text(&r));

    // Disarm and retry — without re-publishing, everything lands.
    stub.fail_all.store(false, Ordering::SeqCst);
    let r = sync_cloud(port, "site", &key);
    assert_eq!(json_num(&r.body, "uploaded"), Some(tree.len() as u64), "retry: {}", body_text(&r));
    let puts = stub.puts();
    let gcs_put = puts.iter().find(|p| p.path.ends_with("index.html")).unwrap();
    let auth = gcs_put.header("authorization").unwrap_or("");
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 Credential=TESTHMAC/")
            && auth.contains("/auto/s3/aws4_request"),
        "{auth}"
    );
    assert_content_types(&puts);

    // And a further sync skips the lot.
    let r = sync_cloud(port, "site", &key);
    assert_eq!(json_num(&r.body, "skipped"), Some(tree.len() as u64), "{}", body_text(&r));
}
