//! End-to-end tests against the real binary, in phext-lattice's test
//! style: spawn the server on an ephemeral port, speak raw HTTP over
//! a TcpStream, and assert on bytes — including the guarantees the
//! design panel named: a re-publish never changes a URL, the version
//! increments, the previous version restores, and the vault is never
//! served.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

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
    // The server prints its bound address on the first stdout line.
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
    headers: String,
    body: Vec<u8>,
}

fn request(port: u16, method: &str, path: &str, headers: &[(&str, String)], body: &[u8]) -> Response {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
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
    Response {
        status,
        headers: head_text,
        body: raw[split + 4..].to_vec(),
    }
}

fn get(port: u16, path: &str) -> Response {
    request(port, "GET", path, &[], &[])
}

/// Build a stored zip in-memory (same format the editor writes).
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
        cen.extend_from_slice(&[0u8; 12]); // extra, comment, disk, attrs
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

fn publish(port: u16, slug: &str, zip: &[u8], key: Option<&str>, changed: Option<&str>) -> Response {
    let mut headers = vec![("X-Slug", slug.to_string())];
    if let Some(k) = key {
        headers.push(("X-Edit-Key", k.to_string()));
    }
    if let Some(c) = changed {
        headers.push(("X-Changed", c.to_string()));
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

fn json_num(body: &[u8], field: &str) -> Option<u64> {
    let text = String::from_utf8_lossy(body);
    let pat = format!("\"{field}\":");
    let start = text.find(&pat)? + pat.len();
    let num: String = text[start..].chars().take_while(|c| c.is_ascii_digit()).collect();
    num.parse().ok()
}

const INDEX_V1: &str = "Dear reader,\n\nThis is where the test bundle begins.\n\n## Notes\n";
const ABOUT: &str = "The short version of the test bundle.\n";

#[test]
fn publish_serve_republish_restore_cycle() {
    let srv = start_server();
    let zip1 = make_zip(&[
        ("index.md", INDEX_V1),
        ("about.md", ABOUT),
        ("essays/one.md", "# One\n\nFirst version.\n"),
        ("vault/secret.md", "# Hidden\n\nNever served.\n"),
    ]);
    let r = publish(srv.port, "test-bundle", &zip1, None, Some("First release"));
    assert_eq!(r.status, 201, "first publish: {}", String::from_utf8_lossy(&r.body));
    let key = json_str(&r.body, "edit_key").expect("first publish returns the key");
    assert_eq!(json_num(&r.body, "version"), Some(1));
    assert_eq!(json_str(&r.body, "url").as_deref(), Some("/b/test-bundle/"));

    // Raw .md serves the source byte-for-byte.
    let raw = get(srv.port, "/b/test-bundle/essays/one.md");
    assert_eq!(raw.status, 200);
    assert_eq!(raw.body, b"# One\n\nFirst version.\n");
    assert!(raw.headers.to_lowercase().contains("text/markdown"));

    // The reading view renders, carries the version line, and no other bundle's marks.
    let view = get(srv.port, "/b/test-bundle/essays/one");
    assert_eq!(view.status, 200);
    let view_text = String::from_utf8_lossy(&view.body).into_owned();
    assert!(view_text.contains("<p>First version.</p>"), "{view_text}");
    assert!(view_text.contains("Version 1 · (Your Version)"), "{view_text}");
    assert!(view_text.contains("First release"), "{view_text}");
    assert!(view_text.contains("read the source"), "{view_text}");
    // The keeper is not on individual pages — the printed page stays clean.
    assert!(!view_text.contains("mascot-badge"), "{view_text}");
    // The quiet suggestion is NOT on individual pages.
    assert!(!view_text.contains("edit.phext.io"), "{view_text}");

    // The index renders the bundle's own index.md, lists a tracklist,
    // names the feed, and carries the quiet line here (and only here).
    let index = get(srv.port, "/b/test-bundle/");
    assert_eq!(index.status, 200);
    let index_text = String::from_utf8_lossy(&index.body).into_owned();
    assert!(index_text.contains("This is where the test bundle begins."), "{index_text}");
    assert!(index_text.contains("feed.xml"), "{index_text}");
    assert!(index_text.contains("edit.phext.io"), "{index_text}");
    assert!(index_text.contains("Version 1"), "{index_text}");
    // …and the keeper stands beside the bundle's title, here on the index.
    assert!(index_text.contains("keeper-badge"), "{index_text}");
    assert!(index_text.contains("/mascot-badge.webp"), "{index_text}");

    // The vault is never served, listed, or fed.
    assert_eq!(get(srv.port, "/b/test-bundle/vault/secret.md").status, 404);
    assert_eq!(get(srv.port, "/b/test-bundle/vault/secret").status, 404);
    assert!(!index_text.contains("secret"), "{index_text}");
    let feed = get(srv.port, "/b/test-bundle/feed.xml");
    assert_eq!(feed.status, 200);
    let feed_text = String::from_utf8_lossy(&feed.body).into_owned();
    assert!(feed_text.contains("<rss version=\"2.0\">"), "{feed_text}");
    assert!(feed_text.contains("/b/test-bundle/essays/one"), "{feed_text}");
    assert!(!feed_text.contains("secret"), "{feed_text}");

    // The bundle downloads as the exact zip that was published.
    let dl = get(srv.port, "/b/test-bundle.zip");
    assert_eq!(dl.status, 200);
    assert_eq!(dl.body, zip1);

    // A wrong key is refused, and nothing changes.
    let zip2 = make_zip(&[
        ("index.md", INDEX_V1),
        ("about.md", ABOUT),
        ("essays/one.md", "# One\n\nSecond version.\n"),
    ]);
    let bad = publish(srv.port, "test-bundle", &zip2, Some("not-the-key"), None);
    assert_eq!(bad.status, 403);
    let still = get(srv.port, "/b/test-bundle/essays/one.md");
    assert_eq!(still.body, b"# One\n\nFirst version.\n");

    // The right key re-publishes: same URL, version 2, content swapped whole.
    let r2 = publish(srv.port, "test-bundle", &zip2, Some(&key), Some("Second release"));
    assert_eq!(r2.status, 200);
    assert_eq!(json_num(&r2.body, "version"), Some(2));
    assert_eq!(json_str(&r2.body, "url").as_deref(), Some("/b/test-bundle/"));
    let now = get(srv.port, "/b/test-bundle/essays/one.md");
    assert_eq!(now.body, b"# One\n\nSecond version.\n");
    let view2 = get(srv.port, "/b/test-bundle/essays/one");
    let view2_text = String::from_utf8_lossy(&view2.body).into_owned();
    assert!(view2_text.contains("Version 2 · (Your Version)"), "{view2_text}");

    // Restore brings the first version back, at the same URL.
    let rr = request(
        srv.port,
        "POST",
        "/api/restore",
        &[("X-Slug", "test-bundle".to_string()), ("X-Edit-Key", key.clone())],
        &[],
    );
    assert_eq!(rr.status, 200, "{}", String::from_utf8_lossy(&rr.body));
    assert_eq!(json_num(&rr.body, "version"), Some(1));
    let back = get(srv.port, "/b/test-bundle/essays/one.md");
    assert_eq!(back.body, b"# One\n\nFirst version.\n");

    // And restoring again returns to version 2 — the swap is two-way.
    let rr2 = request(
        srv.port,
        "POST",
        "/api/restore",
        &[("X-Slug", "test-bundle".to_string()), ("X-Edit-Key", key.clone())],
        &[],
    );
    assert_eq!(rr2.status, 200);
    let fwd = get(srv.port, "/b/test-bundle/essays/one.md");
    assert_eq!(fwd.body, b"# One\n\nSecond version.\n");
}

/// One item's pubDate in a feed: items are single lines, and the
/// item is the one whose link names the file's stem.
fn feed_date(feed: &str, slug: &str, stem: &str) -> String {
    let needle = format!("/b/{slug}/{stem}</link>");
    for line in feed.lines() {
        if line.contains("<item>") && line.contains(&needle) {
            let start = line.find("<pubDate>").expect("item has a pubDate") + "<pubDate>".len();
            let end = line[start..].find("</pubDate>").expect("pubDate closes") + start;
            return line[start..end].to_string();
        }
    }
    panic!("no feed item for {stem} in: {feed}");
}

#[test]
fn feed_dates_follow_each_file_not_the_release() {
    let srv = start_server();
    let zip1 = make_zip(&[("index.md", INDEX_V1), ("about.md", ABOUT)]);
    let r = publish(srv.port, "dated-bundle", &zip1, None, None);
    assert_eq!(r.status, 201, "first publish: {}", String::from_utf8_lossy(&r.body));
    let key = json_str(&r.body, "edit_key").expect("first publish returns the key");
    let feed1 = String::from_utf8_lossy(&get(srv.port, "/b/dated-bundle/feed.xml").body).into_owned();
    let index1 = feed_date(&feed1, "dated-bundle", "index");
    let about1 = feed_date(&feed1, "dated-bundle", "about");
    assert_eq!(index1, about1, "a first publish dates every file alike: {feed1}");

    // Feed dates are seconds; let the clock move so an advance is
    // visible at all.
    std::thread::sleep(Duration::from_millis(1200));
    let zip2 = make_zip(&[
        ("index.md", INDEX_V1),
        ("about.md", "The short version of the test bundle, revised.\n"),
    ]);
    let r2 = publish(srv.port, "dated-bundle", &zip2, Some(&key), None);
    assert_eq!(r2.status, 200, "re-publish: {}", String::from_utf8_lossy(&r2.body));
    let feed2 = String::from_utf8_lossy(&get(srv.port, "/b/dated-bundle/feed.xml").body).into_owned();
    assert_eq!(
        feed_date(&feed2, "dated-bundle", "index"),
        index1,
        "an untouched file keeps its date across a re-release: {feed2}"
    );
    assert_ne!(
        feed_date(&feed2, "dated-bundle", "about"),
        about1,
        "the edited file's date advances: {feed2}"
    );
}

#[test]
fn invalid_bundles_are_refused_whole() {
    let srv = start_server();
    // No markdown at all.
    let r = publish(srv.port, "plain", &make_zip(&[("a.txt", "hi")]), None, None);
    assert_eq!(r.status, 422);
    // A traversal path poisons the whole bundle; nothing is stored.
    let evil = make_zip(&[("index.md", "# Ok\n"), ("../evil.md", "# No\n")]);
    let r = publish(srv.port, "evil", &evil, None, None);
    assert_eq!(r.status, 422);
    assert_eq!(get(srv.port, "/b/evil/").status, 404);
    // Not a zip.
    let r = publish(srv.port, "junk", b"this is not a zip", None, None);
    assert_eq!(r.status, 422);
    // A bad slug never reaches storage.
    let r = publish(srv.port, "Bad Slug", &make_zip(&[("index.md", "# Ok\n")]), None, None);
    assert_eq!(r.status, 400);
}

#[test]
fn editor_and_public_surfaces_answer() {
    let srv = start_server();
    let home = get(srv.port, "/");
    assert_eq!(home.status, 200);
    let home_text = String::from_utf8_lossy(&home.body).into_owned();
    assert!(home_text.contains("Pattern"), "editor names the Pattern face");
    assert!(home_text.contains("Cloth"), "editor names the Cloth face");
    assert!(!home_text.to_lowercase().contains("wizard"), "the interface never says wizard");
    // The keeper's assets answer from the binary itself: the badge
    // crop is a WebP, the artwork serves whole, the favicon is a PNG,
    // and the editor's brand and head name them.
    let badge = get(srv.port, "/mascot-badge.webp");
    assert_eq!(badge.status, 200);
    assert!(badge.body.starts_with(b"RIFF"), "the badge is not a WebP");
    assert_eq!(get(srv.port, "/mascot-pencil-puffball.webp").status, 200);
    let favicon = get(srv.port, "/favicon.png");
    assert_eq!(favicon.status, 200);
    assert!(favicon.body.starts_with(b"\x89PNG"), "the favicon is not a PNG");
    assert!(home_text.contains("/mascot-badge.webp"), "the brand lacks the keeper");
    assert!(home_text.contains("/favicon.png"), "the editor lacks the keeper favicon");
    // The render API is the one renderer behind both faces.
    let rr = request(srv.port, "POST", "/api/render", &[], b"# Title\n\nSome *text*.\n");
    assert_eq!(rr.status, 200);
    let frag = String::from_utf8_lossy(&rr.body).into_owned();
    assert!(frag.contains("<h1>Title</h1>"), "{frag}");
    // Unknown addresses get the quiet 404.
    assert_eq!(get(srv.port, "/no-such-place").status, 404);
    assert_eq!(get(srv.port, "/b/no-such-bundle/").status, 404);
}
