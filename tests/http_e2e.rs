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

/// The public form of a published zip: every entry except the
/// vault, contents byte for byte. The bundle download and the
/// site's inner bundle.zip must equal this — never the published
/// zip itself when the bundle carries a vault (the vault is never
/// published, in any form).
fn assert_public_zip(public: &[u8], published: &[u8]) {
    use std::io::Read as _;
    let mut pub_archive =
        zip::ZipArchive::new(std::io::Cursor::new(public.to_vec())).unwrap();
    let mut pub_names: Vec<String> =
        pub_archive.file_names().map(|n| n.to_string()).collect();
    pub_names.sort();
    let mut src_archive =
        zip::ZipArchive::new(std::io::Cursor::new(published.to_vec())).unwrap();
    let mut expected: Vec<String> = src_archive
        .file_names()
        .filter(|n| !n.starts_with("vault/"))
        .map(|n| n.to_string())
        .collect();
    expected.sort();
    assert_eq!(pub_names, expected, "public zip entries differ");
    for name in &expected {
        let mut a = Vec::new();
        pub_archive
            .by_name(name)
            .unwrap()
            .read_to_end(&mut a)
            .unwrap();
        let mut b = Vec::new();
        src_archive
            .by_name(name)
            .unwrap()
            .read_to_end(&mut b)
            .unwrap();
        assert_eq!(a, b, "public zip entry {name} differs");
    }
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
    let listing = get(srv.port, "/api/bundles/test-bundle");
    assert_eq!(listing.status, 200);
    let listing_text = String::from_utf8_lossy(&listing.body).into_owned();
    assert!(!listing_text.contains("vault"), "the bundle listing names the vault: {listing_text}");
    assert!(listing_text.contains("essays/one.md"), "{listing_text}");
    assert_eq!(get(srv.port, "/b/test-bundle/vault/secret.md").status, 404);
    assert_eq!(get(srv.port, "/b/test-bundle/vault/secret").status, 404);
    assert!(!index_text.contains("secret"), "{index_text}");
    let feed = get(srv.port, "/b/test-bundle/feed.xml");
    assert_eq!(feed.status, 200);
    let feed_text = String::from_utf8_lossy(&feed.body).into_owned();
    assert!(feed_text.contains("<rss version=\"2.0\""), "{feed_text}");
    assert!(feed_text.contains("/b/test-bundle/essays/one"), "{feed_text}");
    assert!(!feed_text.contains("secret"), "{feed_text}");

    // The bundle downloads as its public zip: the published
    // entries with the vault left out.
    let dl = get(srv.port, "/b/test-bundle.zip");
    assert_eq!(dl.status, 200);
    assert_public_zip(&dl.body, &zip1);

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

/// The `<article>…</article>` heart of a rendered page.
fn article_of(html: &str) -> String {
    let start = html.find("<article>").expect("page has an article") + "<article>".len();
    let end = html[start..].find("</article>").expect("article closes") + start;
    html[start..end].to_string()
}

/// Racing first publishes of one fresh slug: exactly one is
/// Created and mints the key that opens the slug; the rest are
/// refused. (Before the store's per-slug commit lock, every racer
/// passed the "unpublished" check, every one answered 201 with a
/// distinct key, and all but one key opened nothing.)
#[test]
fn racing_first_publishes_mint_one_key() {
    let srv = start_server();
    let port = srv.port;
    let zip = make_zip(&[("index.md", INDEX_V1)]);
    let mut handles = Vec::new();
    for _ in 0..8 {
        let zip = zip.clone();
        handles.push(std::thread::spawn(move || {
            publish(port, "race-bundle", &zip, None, None)
        }));
    }
    let mut keys = Vec::new();
    let mut refused = 0;
    for h in handles {
        let r = h.join().unwrap();
        if r.status == 201 {
            keys.push(json_str(&r.body, "edit_key").expect("a creation returns its key"));
        } else {
            assert_eq!(r.status, 403, "a racing loser answered {}", r.status);
            refused += 1;
        }
    }
    assert_eq!(keys.len(), 1, "exactly one racer creates the slug");
    assert_eq!(refused, 7);
    let zip2 = make_zip(&[("index.md", "# Again\n")]);
    let r = publish(srv.port, "race-bundle", &zip2, Some(&keys[0]), None);
    assert_eq!(r.status, 200, "the one minted key opens the slug");
}

/// The derived outputs come with the publish: a pre-rendered page
/// and a PDF at each file's `.html` / `.pdf` address, and the whole
/// bundle as a deployable static site at `site.zip` — all rebuilt
/// on re-publish and on restore, the vault in none of them.
#[test]
fn outputs_come_with_the_publish() {
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

    // The pre-rendered page is the reading view's own rendering,
    // served as a static file.
    let page = get(srv.port, "/b/test-bundle/essays/one.html");
    assert_eq!(page.status, 200);
    let page_text = String::from_utf8_lossy(&page.body).into_owned();
    let view = get(srv.port, "/b/test-bundle/essays/one");
    let view_text = String::from_utf8_lossy(&view.body).into_owned();
    assert_eq!(
        article_of(&page_text),
        article_of(&view_text),
        "the .html page's article is the reading view's rendering"
    );
    assert!(page_text.contains("Version 1 · (Your Version)"), "{page_text}");

    // The PDF is a real PDF when this host has a Chromium to print
    // with — and the plainly-worded unavailable page when it has
    // not. Both are designed states; anything else is a failure.
    let pdf = get(srv.port, "/b/test-bundle/essays/one.pdf");
    let pdf_available = if pdf.status == 200 {
        assert!(pdf.body.starts_with(b"%PDF-"), "the .pdf address serves PDF bytes");
        assert!(pdf.body.len() > 1000, "the PDF is a real printed page, not a stub");
        assert!(
            pdf.headers.to_lowercase().contains("application/pdf"),
            "pdf content type: {}",
            pdf.headers
        );
        true
    } else {
        assert_eq!(pdf.status, 404, "pdf answered {}", pdf.status);
        let text = String::from_utf8_lossy(&pdf.body).into_owned();
        assert!(text.contains("no Chromium"), "the unavailable page says why: {text}");
        false
    };

    // The static site: the whole tree in one zip, links rewritten
    // to live inside it.
    let site = get(srv.port, "/b/test-bundle/site.zip");
    assert_eq!(site.status, 200);
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(site.body)).unwrap();
    let mut names: Vec<String> = archive.file_names().map(|n| n.to_string()).collect();
    names.sort();
    for expected in [
        "index.html",
        "about.html",
        "essays/one.html",
        "index.md",
        "about.md",
        "essays/one.md",
        "feed.xml",
        "bundle.zip",
        "README.txt",
        "favicon.svg",
        "favicon.png",
        "mascot-badge.webp",
    ] {
        assert!(names.iter().any(|n| n == expected), "site.zip lacks {expected}: {names:?}");
    }
    if pdf_available {
        assert!(names.iter().any(|n| n == "essays/one.pdf"), "site.zip lacks the PDF: {names:?}");
    }
    assert!(
        !names.iter().any(|n| n.starts_with("vault/")),
        "the vault is in the site download: {names:?}"
    );
    // The source archive rides inside — the public zip, the
    // published entries with the vault left out.
    let mut inner = String::new();
    {
        let mut entry = archive.by_name("bundle.zip").unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
        assert_public_zip(&bytes, &zip1);
    }
    // The exported index links resolve inside the tree: relative,
    // and at the tree's own addresses.
    {
        let mut entry = archive.by_name("index.html").unwrap();
        std::io::Read::read_to_string(&mut entry, &mut inner).unwrap();
    }
    assert!(inner.contains("href=\"essays/one.html\""), "export tracklist: {inner}");
    assert!(inner.contains("href=\"bundle.zip\""), "export colophon: {inner}");
    assert!(inner.contains("href=\"feed.xml\""), "export feed link: {inner}");
    assert!(!inner.contains("href=\"/b/"), "an absolute bundle link survived the rewrite: {inner}");
    assert!(!inner.contains("site.zip"), "the deploy link names itself in the export: {inner}");
    assert!(!inner.contains("secret"), "the export index lists the vault: {inner}");

    // The vault has no outputs either.
    assert_eq!(get(srv.port, "/b/test-bundle/vault/secret.html").status, 404);
    assert_eq!(get(srv.port, "/b/test-bundle/vault/secret.pdf").status, 404);

    // Re-publish rebuilds the outputs at the same addresses.
    let zip2 = make_zip(&[
        ("index.md", INDEX_V1),
        ("about.md", ABOUT),
        ("essays/one.md", "# One\n\nSecond version.\n"),
    ]);
    let r2 = publish(srv.port, "test-bundle", &zip2, Some(&key), Some("Second release"));
    assert_eq!(r2.status, 200);
    let page2 = get(srv.port, "/b/test-bundle/essays/one.html");
    let page2_text = String::from_utf8_lossy(&page2.body).into_owned();
    assert!(page2_text.contains("Second version."), "{page2_text}");
    assert!(page2_text.contains("Version 2 · (Your Version)"), "{page2_text}");

    // And a restore rebuilds them back — outputs follow the commit
    // in both directions.
    let rr = request(
        srv.port,
        "POST",
        "/api/restore",
        &[("X-Slug", "test-bundle".to_string()), ("X-Edit-Key", key.clone())],
        &[],
    );
    assert_eq!(rr.status, 200);
    let page3 = get(srv.port, "/b/test-bundle/essays/one.html");
    let page3_text = String::from_utf8_lossy(&page3.body).into_owned();
    assert!(page3_text.contains("First version."), "{page3_text}");
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

#[test]
fn integrate_page_is_served_linked_and_kept_by_the_mascot() {
    let srv = start_server();
    let page = get(srv.port, "/integrate");
    assert_eq!(page.status, 200);
    let text = String::from_utf8_lossy(&page.body).into_owned();
    for needle in [
        "https://md.phext.io/mcp",
        "X-Edit-Key",
        "application password",
        "Your own hosting",
        "phext vault",
        "https://tluf.org",
    ] {
        assert!(text.contains(needle), "integrate page lacks {needle}");
    }
    // The mascot answers as the webp it is.
    let mascot = get(srv.port, "/mascot-badge.webp");
    assert_eq!(mascot.status, 200);
    assert!(mascot.body.starts_with(b"RIFF"), "mascot is not the webp");
    assert!(mascot.headers.contains("image/webp"), "mascot served as {}", mascot.headers);
    // The editor links the page (chrome and side-foot).
    let home = get(srv.port, "/");
    assert!(
        String::from_utf8_lossy(&home.body).contains("href=\"/integrate\""),
        "editor does not link the integrate page"
    );
    // And so does every bundle index's footer.
    let zip = make_zip(&[("index.md", "# Linked\n"), ("one.md", "# One\n")]);
    let r = publish(srv.port, "linked", &zip, None, None);
    assert_eq!(r.status, 201);
    let index = get(srv.port, "/b/linked/");
    assert_eq!(index.status, 200);
    assert!(
        String::from_utf8_lossy(&index.body).contains("href=\"/integrate\""),
        "bundle index does not link the integrate page"
    );
}

/// One response header's value, by case-insensitive name.
fn header_value(headers: &str, name: &str) -> Option<String> {
    headers.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        if k.trim().eq_ignore_ascii_case(name) {
            Some(v.trim().to_string())
        } else {
            None
        }
    })
}

/// The text between two markers of a feed or item block.
fn element(text: &str, open: &str, close: &str) -> String {
    let start = text
        .find(open)
        .unwrap_or_else(|| panic!("{open} missing in: {text}"))
        + open.len();
    let end = text[start..]
        .find(close)
        .unwrap_or_else(|| panic!("{close} missing in: {text}"))
        + start;
    text[start..end].to_string()
}

/// The `<item>` block whose link ends with the given tail.
fn item_block(feed: &str, link_tail: &str) -> String {
    for chunk in feed.split("<item>").skip(1) {
        let end = chunk.find("</item>").expect("item closes");
        let block = &chunk[..end];
        if block.contains(link_tail) {
            return block.to_string();
        }
    }
    panic!("no item for {link_tail} in: {feed}");
}

/// Reverse `xml_escape` — what a feed reader's parser hands back.
fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The escaping discipline, checked mechanically: every `&` in
/// the document opens one of the five entities.
fn assert_no_bare_ampersands(feed: &str) {
    for (i, b) in feed.bytes().enumerate() {
        if b == b'&' {
            let rest = &feed[i..];
            assert!(
                ["&amp;", "&lt;", "&gt;", "&quot;", "&apos;"]
                    .iter()
                    .any(|e| rest.starts_with(e)),
                "bare & at byte {i} in: {feed}"
            );
        }
    }
}

/// The enriched feeds, end to end against the real server: an
/// item carries the whole rendering and the byte-exact source,
/// its GUID survives a retitle, the vault is absent from both
/// feeds, both feeds answer conditional GETs, the published
/// pages carry autodiscovery, and the site-wide feed merges
/// every bundle newest first.
#[test]
fn feeds_are_full_cacheable_and_merged() {
    let srv = start_server();
    let one_md = "# One\n\nFirst paragraph, with AT&T and <angle> brackets.\n\nSee [two](two.md) and ![pic](pic.png).\n\nA middle paragraph that pads the post out to a length where a short description must stop long before the end of the text.\n\nAnother middle paragraph, because one was not quite enough padding to pass two hundred characters of prose.\n\nThe final paragraph, which an excerpt would drop.\n";
    let zip1 = make_zip(&[
        ("index.md", "# Home\n\nThe feed test bundle.\n"),
        ("essays/one.md", one_md),
        ("essays/two.md", "# Two\n\nThe second file.\n"),
        ("essays/pic.png", "fakepng"),
        ("vault/secret.md", "# Hidden\n\nNever fed.\n"),
    ]);
    let r = publish(srv.port, "feed-one", &zip1, None, Some("First release"));
    assert_eq!(r.status, 201, "first publish: {}", String::from_utf8_lossy(&r.body));
    let key = json_str(&r.body, "edit_key").expect("first publish returns the key");

    let feed = get(srv.port, "/b/feed-one/feed.xml");
    assert_eq!(feed.status, 200);
    let etag = header_value(&feed.headers, "etag").expect("the feed carries an ETag");
    let text = String::from_utf8_lossy(&feed.body).into_owned();
    assert_no_bare_ampersands(&text);
    assert!(
        text.contains("xmlns:source=\"http://source.scripting.com/\""),
        "{text}"
    );
    // The item for essays/one: source byte-exact, content whole,
    // addresses absolute.
    let item = item_block(&text, "/b/feed-one/essays/one");
    let source = xml_unescape(&element(&item, "<source:markdown>", "</source:markdown>"));
    assert_eq!(source, one_md, "source:markdown must be the file, byte for byte");
    let content = xml_unescape(&element(&item, "<content:encoded>", "</content:encoded>"));
    assert!(content.contains("The final paragraph"), "{content}");
    assert!(
        content.contains(&format!(
            "href=\"http://127.0.0.1:{}/b/feed-one/essays/two\"",
            srv.port
        )),
        "{content}"
    );
    assert!(
        content.contains(&format!(
            "src=\"http://127.0.0.1:{}/b/feed-one/essays/pic.png\"",
            srv.port
        )),
        "{content}"
    );
    let guid_v1 = element(&item, "<guid isPermaLink=\"true\">", "</guid>");
    assert_eq!(
        guid_v1,
        format!("http://127.0.0.1:{}/b/feed-one/essays/one", srv.port)
    );
    // The vault is not fed — not its path, not its words.
    assert!(!text.contains("secret"), "{text}");
    assert!(!text.contains("Never fed."), "{text}");

    // Autodiscovery rides the published pages: index and reading
    // view alike name the bundle's feed.
    let index = get(srv.port, "/b/feed-one/");
    let index_text = String::from_utf8_lossy(&index.body).into_owned();
    assert!(
        index_text.contains("<link rel=\"alternate\" type=\"application/rss+xml\""),
        "{index_text}"
    );
    assert!(index_text.contains("/b/feed-one/feed.xml"), "{index_text}");
    let view = get(srv.port, "/b/feed-one/essays/one");
    let view_text = String::from_utf8_lossy(&view.body).into_owned();
    assert!(
        view_text.contains("<link rel=\"alternate\" type=\"application/rss+xml\""),
        "{view_text}"
    );

    // A conditional GET on an unchanged feed is a 304.
    let cond = request(
        srv.port,
        "GET",
        "/b/feed-one/feed.xml",
        &[("If-None-Match", etag.clone())],
        &[],
    );
    assert_eq!(cond.status, 304, "an unchanged feed must revalidate: {}", cond.headers);
    assert!(cond.body.is_empty());

    // A retitle changes the item's title, never its GUID — and
    // the changed feed no longer answers the old ETag.
    std::thread::sleep(Duration::from_millis(1200));
    let renamed = one_md.replacen("# One", "# One, Renamed", 1);
    let zip2 = make_zip(&[
        ("index.md", "# Home\n\nThe feed test bundle.\n"),
        ("essays/one.md", renamed.as_str()),
        ("essays/two.md", "# Two\n\nThe second file.\n"),
        ("essays/pic.png", "fakepng"),
        ("vault/secret.md", "# Hidden\n\nNever fed.\n"),
    ]);
    let r2 = publish(srv.port, "feed-one", &zip2, Some(&key), None);
    assert_eq!(r2.status, 200, "re-publish: {}", String::from_utf8_lossy(&r2.body));
    let feed2 = get(srv.port, "/b/feed-one/feed.xml");
    let text2 = String::from_utf8_lossy(&feed2.body).into_owned();
    let item2 = item_block(&text2, "/b/feed-one/essays/one");
    assert_eq!(element(&item2, "<guid isPermaLink=\"true\">", "</guid>"), guid_v1);
    assert!(item2.contains("<title>One, Renamed</title>"), "{item2}");
    let stale = request(
        srv.port,
        "GET",
        "/b/feed-one/feed.xml",
        &[("If-None-Match", etag)],
        &[],
    );
    assert_eq!(stale.status, 200, "a stale ETag must not 304 a changed feed");

    // The site-wide feed: a second bundle, published later,
    // leads it; both bundles' items are present; the vault is
    // absent; it revalidates like the bundle feed.
    std::thread::sleep(Duration::from_millis(1200));
    let zip3 = make_zip(&[("index.md", "# Later\n\nThe newest bundle.\n")]);
    let r3 = publish(srv.port, "feed-two", &zip3, None, None);
    assert_eq!(r3.status, 201);
    let site = get(srv.port, "/rss.xml");
    assert_eq!(site.status, 200);
    let site_text = String::from_utf8_lossy(&site.body).into_owned();
    assert_no_bare_ampersands(&site_text);
    let two_pos = site_text
        .find("/b/feed-two/index")
        .expect("the site feed lacks feed-two");
    let one_pos = site_text
        .find("/b/feed-one/essays/one")
        .expect("the site feed lacks feed-one");
    assert!(two_pos < one_pos, "the site-wide feed is newest first: {site_text}");
    assert!(!site_text.contains("secret"), "{site_text}");
    assert!(!site_text.contains("Never fed."), "{site_text}");
    let site_etag = header_value(&site.headers, "etag").expect("the site feed carries an ETag");
    let site_cond = request(
        srv.port,
        "GET",
        "/rss.xml",
        &[("If-None-Match", site_etag)],
        &[],
    );
    assert_eq!(site_cond.status, 304);
}

/// A server started with extra environment — the WebSub test
/// needs a hub configured, and configuration is an env var read
/// once at first use, so it must be present from the start.
fn start_server_env(envs: &[(&str, &str)]) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_markdown-wizard"));
    cmd.args(["--dir", dir.path().to_str().unwrap(), "--port", "0", "--host", "127.0.0.1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
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

/// WebSub, end to end: with a hub configured, the feed carries
/// the hub link, and a publish pings the hub for both changed
/// feed addresses — the bundle's own and the site-wide feed.
/// The stub hub records what it is sent; the publish itself is
/// already home before the pings arrive, which is the point:
/// a hub can never hold a publish hostage.
#[test]
fn websub_pings_the_hub_on_publish() {
    use std::sync::{Arc as StdArc, Mutex as StdMutex};
    // The stub hub: accept, record the body, answer 200.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let hub_port = listener.local_addr().unwrap().port();
    let inbox: StdArc<StdMutex<Vec<String>>> = StdArc::new(StdMutex::new(Vec::new()));
    let inbox_thread = inbox.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            // Read until the declared body has arrived.
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        raw.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&raw);
                        if let Some(head_end) = text.find("\r\n\r\n") {
                            let len: usize = text[..head_end]
                                .lines()
                                .find_map(|l| {
                                    l.to_lowercase()
                                        .strip_prefix("content-length:")
                                        .and_then(|v| v.trim().parse().ok())
                                })
                                .unwrap_or(0);
                            if text.len() >= head_end + 4 + len {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            let text = String::from_utf8_lossy(&raw).into_owned();
            let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            inbox_thread.lock().unwrap().push(body);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = stream.flush();
        }
    });
    let hub_url = format!("http://127.0.0.1:{hub_port}/hub");
    let srv = start_server_env(&[("MDWIZ_WEBSUB_HUB", &hub_url)]);

    let zip = make_zip(&[("index.md", "# Hubbed\n\nAnnounced in real time.\n")]);
    let r = publish(srv.port, "hub-bundle", &zip, None, None);
    assert_eq!(r.status, 201, "publish: {}", String::from_utf8_lossy(&r.body));

    // The feed names the hub (and itself).
    let feed = get(srv.port, "/b/hub-bundle/feed.xml");
    let feed_text = String::from_utf8_lossy(&feed.body).into_owned();
    assert!(
        feed_text.contains(&format!("<atom:link href=\"{hub_url}\" rel=\"hub\"/>")),
        "{feed_text}"
    );

    // Both pings arrive: the bundle feed and the site-wide feed.
    let decode = |s: &str| s.replace("%3A", ":").replace("%2F", "/");
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        let got: Vec<String> = inbox.lock().unwrap().clone();
        let decoded: Vec<String> = got.iter().map(|b| decode(b)).collect();
        let bundle_ping = decoded
            .iter()
            .any(|b| b.contains("hub.mode=publish") && b.contains("hub.url=https://md.phext.io/b/hub-bundle/feed.xml"));
        let site_ping = decoded
            .iter()
            .any(|b| b.contains("hub.mode=publish") && b.contains("hub.url=https://md.phext.io/rss.xml"));
        if bundle_ping && site_ping {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the hub saw: {decoded:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
