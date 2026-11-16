//! The MCP endpoint, end to end, in the house style of
//! tests/http_e2e.rs: spawn the real binary, speak raw HTTP, and
//! drive a scripted MCP client session — initialize, list tools,
//! stream writes into a working copy, search, publish, and verify
//! the public addresses carry the agent's edits. Plus the two
//! refusals that matter: a wrong key, and a stale draft that must
//! never overwrite a newer release.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

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
    Response { status, body: raw[split + 4..].to_vec() }
}

fn get(port: u16, path: &str) -> Response {
    request(port, "GET", path, &[], &[])
}

/// One JSON-RPC call over POST /mcp. Returns (status, parsed body).
fn rpc(port: u16, req: &Value, key_header: Option<&str>) -> (u16, Value) {
    let mut headers = vec![("Content-Type", "application/json".to_string())];
    if let Some(k) = key_header {
        headers.push(("X-Edit-Key", k.to_string()));
    }
    let body = serde_json::to_vec(req).unwrap();
    let r = request(port, "POST", "/mcp", &headers, &body);
    let parsed = if r.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&r.body).unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&r.body)))
    };
    (r.status, parsed)
}

fn call(port: u16, id: u64, name: &str, args: Value, key_header: Option<&str>) -> Value {
    let (status, resp) = rpc(
        port,
        &json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}}),
        key_header,
    );
    assert_eq!(status, 200, "tools/call {name}: {resp}");
    resp["result"].clone()
}

/// The tool result's payload: content[0].text, itself JSON on success.
fn tool_json(result: &Value) -> Value {
    assert_eq!(result["isError"], json!(false), "tool errored: {result}");
    let text = result["content"][0]["text"].as_str().expect("text content");
    serde_json::from_str(text).unwrap_or_else(|_| panic!("tool text is not JSON: {text}"))
}

fn tool_error(result: &Value) -> String {
    assert_eq!(result["isError"], json!(true), "tool should have errored: {result}");
    result["content"][0]["text"].as_str().unwrap_or("").to_string()
}

// ——— the web publish path, for seeding slugs the agent then edits ———

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

fn web_publish(port: u16, slug: &str, zip: &[u8], key: Option<&str>) -> (u16, Value) {
    let mut headers = vec![("X-Slug", slug.to_string())];
    if let Some(k) = key {
        headers.push(("X-Edit-Key", k.to_string()));
    }
    let r = request(port, "POST", "/api/publish", &headers, zip);
    let parsed = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
    (r.status, parsed)
}

#[test]
fn mcp_agent_session_end_to_end() {
    let srv = start_server();

    // A GET probe learns the endpoint is alive: 405, in JSON.
    let probe = get(srv.port, "/mcp");
    assert_eq!(probe.status, 405);
    let probe_json: Value = serde_json::from_slice(&probe.body).unwrap();
    assert_eq!(probe_json["error"]["code"], json!(-32000));

    // initialize negotiates the client's version.
    let (status, init) = rpc(
        srv.port,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "e2e-agent", "version": "0"}
        }}),
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(init["result"]["protocolVersion"], json!("2025-03-26"));
    assert_eq!(init["result"]["serverInfo"]["name"], json!("markdown-wizard"));

    // A notification is answered with silence: 202, no body.
    let (status, _) = rpc(
        srv.port,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        None,
    );
    assert_eq!(status, 202);

    // ping, and an unknown method as a protocol error.
    let (_, pong) = rpc(srv.port, &json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}), None);
    assert_eq!(pong["result"], json!({}));
    let (_, nope) = rpc(srv.port, &json!({"jsonrpc": "2.0", "id": 3, "method": "no/such"}), None);
    assert_eq!(nope["error"]["code"], json!(-32601));

    // tools/list carries exactly the six bundle tools.
    let (_, list) = rpc(srv.port, &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"}), None);
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["list_bundles", "list_files", "read_file", "search_bundle", "write_file", "publish_bundle"]
    );

    // The first write to a fresh slug mints its edit key, shown once.
    let w1 = tool_json(&call(
        srv.port,
        5,
        "write_file",
        json!({"slug": "agent-notes", "path": "index.md", "content": "# Agent Notes\n\nKept by an agent.\n"}),
        None,
    ));
    let key = w1["edit_key"].as_str().expect("first write mints the key").to_string();
    assert_eq!(w1["base_version"], json!(0));
    assert_eq!(w1["diverged"], json!(true));

    // A second file streams in; the key rides the argument here.
    let w2 = tool_json(&call(
        srv.port,
        6,
        "write_file",
        json!({"slug": "agent-notes", "path": "essays/first.md", "content": "# First\n\nWritten by the agent.\n", "edit_key": key}),
        None,
    ));
    assert_eq!(w2["files"], json!(2));
    assert!(w2.get("edit_key").is_none(), "the key is shown once, not per write");

    // A wrong key is a clean refusal, and the draft is untouched.
    let bad = tool_error(&call(
        srv.port,
        7,
        "write_file",
        json!({"slug": "agent-notes", "path": "essays/first.md", "content": "# Hijack\n", "edit_key": "not-the-key"}),
        None,
    ));
    assert!(bad.contains("does not open"), "{bad}");
    let still = tool_json(&call(
        srv.port,
        8,
        "read_file",
        json!({"slug": "agent-notes", "path": "essays/first.md", "draft": true, "edit_key": key}),
        None,
    ));
    assert!(still["content"].as_str().unwrap().contains("Written by the agent."));

    // Draft reads demand the key; published reads find nothing yet.
    let no_key = tool_error(&call(
        srv.port,
        9,
        "read_file",
        json!({"slug": "agent-notes", "path": "index.md", "draft": true}),
        None,
    ));
    assert!(no_key.contains("does not open"), "{no_key}");
    let unpublished = tool_error(&call(
        srv.port,
        10,
        "read_file",
        json!({"slug": "agent-notes", "path": "index.md"}),
        None,
    ));
    assert!(unpublished.contains("no published bundle"), "{unpublished}");

    // Search the draft for the agent's own words.
    let found = tool_json(&call(
        srv.port,
        11,
        "search_bundle",
        json!({"slug": "agent-notes", "query": "agent", "draft": true, "edit_key": key}),
        None,
    ));
    let hits = found["hits"].as_array().unwrap();
    assert!(hits.iter().any(|h| h["path"] == json!("essays/first.md")), "{found}");

    // A batch streams two writes in one request; the key rides the
    // header this time.
    let (status, batch) = rpc(
        srv.port,
        &json!([
            {"jsonrpc": "2.0", "id": 12, "method": "tools/call", "params": {"name": "write_file", "arguments": {"slug": "agent-notes", "path": "notes/todo.md", "content": "# Todo\n\n- publish\n"}}},
            {"jsonrpc": "2.0", "id": 13, "method": "tools/call", "params": {"name": "write_file", "arguments": {"slug": "agent-notes", "path": "about.md", "content": "The agent's bundle.\n"}}}
        ]),
        Some(&key),
    );
    assert_eq!(status, 200);
    let replies = batch.as_array().unwrap();
    assert_eq!(replies.len(), 2);
    for r in replies {
        assert_eq!(r["result"]["isError"], json!(false), "{r}");
    }

    // Publish: the draft becomes the bundle, at its stable URL.
    let published = tool_json(&call(
        srv.port,
        14,
        "publish_bundle",
        json!({"slug": "agent-notes", "changed": "Agent release", "edit_key": key}),
        None,
    ));
    assert_eq!(published["version"], json!(1));
    assert_eq!(published["url"], json!("/b/agent-notes/"));

    // The public addresses carry the agent's edits.
    let raw = get(srv.port, "/b/agent-notes/essays/first.md");
    assert_eq!(raw.status, 200);
    assert_eq!(raw.body, b"# First\n\nWritten by the agent.\n");
    let view = get(srv.port, "/b/agent-notes/essays/first");
    let view_text = String::from_utf8_lossy(&view.body).into_owned();
    assert!(view_text.contains("Version 1 · (Your Version)"), "{view_text}");
    assert!(view_text.contains("Agent release"), "{view_text}");

    // list_bundles sees it; the draft now sits Current on version 1.
    let bundles = tool_json(&call(srv.port, 15, "list_bundles", json!({}), None));
    let mine = bundles["bundles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["slug"] == json!("agent-notes"))
        .expect("the agent's bundle is listed")
        .clone();
    assert_eq!(mine["version"], json!(1));
    assert_eq!(mine["has_working_copy"], json!(true));
    let draft_list = tool_json(&call(
        srv.port,
        16,
        "list_files",
        json!({"slug": "agent-notes", "draft": true, "edit_key": key}),
        None,
    ));
    assert_eq!(draft_list["base_version"], json!(1));
    assert_eq!(draft_list["diverged"], json!(false));

    // One more streamed edit, one more publish: version 2, same URL.
    tool_json(&call(
        srv.port,
        17,
        "write_file",
        json!({"slug": "agent-notes", "path": "essays/first.md", "content": "# First\n\nRevised by the agent.\n", "edit_key": key}),
        None,
    ));
    let republished = tool_json(&call(
        srv.port,
        18,
        "publish_bundle",
        json!({"slug": "agent-notes", "edit_key": key}),
        None,
    ));
    assert_eq!(republished["version"], json!(2));
    assert_eq!(republished["url"], json!("/b/agent-notes/"));
    let raw2 = get(srv.port, "/b/agent-notes/essays/first.md");
    assert_eq!(raw2.body, b"# First\n\nRevised by the agent.\n");
}

#[test]
fn mcp_stale_draft_cannot_overwrite_a_newer_release() {
    let srv = start_server();
    // A human publishes first, over the web.
    let zip1 = make_zip(&[("index.md", "# Race\n\nHuman version one.\n")]);
    let (status, r1) = web_publish(srv.port, "race", &zip1, None);
    assert_eq!(status, 201);
    let key = r1["edit_key"].as_str().unwrap().to_string();

    // The agent opens a working copy seeded from version 1 and edits.
    let w = tool_json(&call(
        srv.port,
        1,
        "write_file",
        json!({"slug": "race", "path": "index.md", "content": "# Race\n\nAgent edit on version one.\n", "edit_key": key}),
        None,
    ));
    assert_eq!(w["base_version"], json!(1));

    // The human re-publishes version 2 underneath the draft.
    let zip2 = make_zip(&[("index.md", "# Race\n\nHuman version two.\n")]);
    let (status, r2) = web_publish(srv.port, "race", &zip2, Some(&key));
    assert_eq!(status, 200);
    assert_eq!(r2["version"], json!(2));

    // The agent's publish is refused as stale — cleanly, with both
    // versions named — and the public bundle stays at version 2.
    let stale = tool_error(&call(
        srv.port,
        2,
        "publish_bundle",
        json!({"slug": "race", "edit_key": key}),
        None,
    ));
    assert!(stale.contains("version 2") && stale.contains("version 1"), "{stale}");
    let raw = get(srv.port, "/b/race/index.md");
    assert_eq!(raw.body, b"# Race\n\nHuman version two.\n");

    // The draft itself survives the refusal, readable by its key.
    let draft = tool_json(&call(
        srv.port,
        3,
        "read_file",
        json!({"slug": "race", "path": "index.md", "draft": true, "edit_key": key}),
        None,
    ));
    assert!(draft["content"].as_str().unwrap().contains("Agent edit on version one."));
}

#[test]
fn mcp_published_reads_are_public_and_the_vault_stays_private() {
    let srv = start_server();
    let zip = make_zip(&[
        ("index.md", "# Public\n\nOpen to every reader.\n"),
        ("vault/secret.md", "# Hidden\n\nNever served.\n"),
    ]);
    let (status, _) = web_publish(srv.port, "pub", &zip, None);
    assert_eq!(status, 201);

    // Published reads need no key at all.
    let read = tool_json(&call(
        srv.port,
        1,
        "read_file",
        json!({"slug": "pub", "path": "index.md"}),
        None,
    ));
    assert!(read["content"].as_str().unwrap().contains("Open to every reader."));
    let listed = tool_json(&call(srv.port, 2, "list_files", json!({"slug": "pub"}), None));
    let paths: Vec<&str> = listed["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["index.md"]);

    // The vault is not read, listed, or found by search — it is not
    // distinguished from a file that was never there.
    let vault = tool_error(&call(
        srv.port,
        3,
        "read_file",
        json!({"slug": "pub", "path": "vault/secret.md"}),
        None,
    ));
    assert!(vault.contains("no file"), "{vault}");
    let search = tool_json(&call(
        srv.port,
        4,
        "search_bundle",
        json!({"slug": "pub", "query": "Hidden"}),
        None,
    ));
    assert_eq!(search["hits"].as_array().unwrap().len(), 0);

    // And a keyless write to a published slug is refused.
    let write = tool_error(&call(
        srv.port,
        5,
        "write_file",
        json!({"slug": "pub", "path": "index.md", "content": "# Taken\n"}),
        None,
    ));
    assert!(write.contains("does not open"), "{write}");
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// The keep-private mark, end to end: a marked file stays in the
/// working copy — listed there with its mark, readable by the key
/// holder — and is absent from every published surface, as if it
/// did not exist. Un-marking restores it; marking it again after
/// it was live removes it, because a publish replaces the whole
/// published tree.
#[test]
fn mcp_keep_private_mark_holds_a_file_out_of_the_publication() {
    let srv = start_server();
    let slug = "marked-bundle";

    // Three files; the middle one is marked as it is written. The
    // first write mints the slug's edit key.
    let w1 = tool_json(&call(
        srv.port,
        1,
        "write_file",
        json!({"slug": slug, "path": "index.md", "content": "# Marked Bundle\n\nOpen to every reader.\n"}),
        None,
    ));
    let key = w1["edit_key"].as_str().expect("first write mints the key").to_string();
    assert_eq!(w1["keep_private"], json!(false));
    let w2 = tool_json(&call(
        srv.port,
        2,
        "write_file",
        json!({"slug": slug, "path": "secret-draft.md", "content": "# Secret Draft\n\nA unicorn hides here.\n", "keep_private": true, "edit_key": key}),
        None,
    ));
    assert_eq!(w2["keep_private"], json!(true));
    let w3 = tool_json(&call(
        srv.port,
        3,
        "write_file",
        json!({"slug": slug, "path": "essays/open.md", "content": "# Open Essay\n\nAlso public.\n", "edit_key": key}),
        None,
    ));
    assert_eq!(w3["keep_private"], json!(false));

    // The draft listing shows the mark, file by file.
    let draft = tool_json(&call(
        srv.port,
        4,
        "list_files",
        json!({"slug": slug, "draft": true, "edit_key": key}),
        None,
    ));
    let marks: Vec<(&str, bool)> = draft["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["path"].as_str().unwrap(), f["keep_private"].as_bool().unwrap()))
        .collect();
    assert!(marks.contains(&("secret-draft.md", true)), "{marks:?}");
    assert!(marks.contains(&("index.md", false)), "{marks:?}");
    assert!(marks.contains(&("essays/open.md", false)), "{marks:?}");

    // Publish: one atomic act over the unmarked set, and the
    // result names what was held back.
    let pub1 = tool_json(&call(
        srv.port,
        5,
        "publish_bundle",
        json!({"slug": slug, "edit_key": key}),
        None,
    ));
    assert_eq!(pub1["version"], json!(1));
    assert_eq!(pub1["kept_private"], json!(["secret-draft.md"]));
    let published_paths: Vec<&str> = pub1["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert!(!published_paths.contains(&"secret-draft.md"), "{published_paths:?}");
    // The draft, marks and all, agrees with the published bundle:
    // a held-back file is not a divergence.
    let draft_after = tool_json(&call(
        srv.port,
        6,
        "list_files",
        json!({"slug": slug, "draft": true, "edit_key": key}),
        None,
    ));
    assert_eq!(draft_after["diverged"], json!(false), "{draft_after}");

    // The published surface: as if the file does not exist.
    assert_eq!(get(srv.port, "/b/marked-bundle/secret-draft.md").status, 404);
    assert_eq!(get(srv.port, "/b/marked-bundle/secret-draft").status, 404);
    assert_eq!(get(srv.port, "/b/marked-bundle/secret-draft.html").status, 404);
    assert_eq!(get(srv.port, "/b/marked-bundle/secret-draft.pdf").status, 404);
    let index = get(srv.port, "/b/marked-bundle/");
    assert_eq!(index.status, 200);
    let index_text = String::from_utf8_lossy(&index.body).into_owned();
    assert!(!index_text.contains("secret-draft"), "{index_text}");
    let feed_text =
        String::from_utf8_lossy(&get(srv.port, "/b/marked-bundle/feed.xml").body).into_owned();
    assert!(!feed_text.contains("secret-draft"), "{feed_text}");
    // The site-wide feed is built from the same published bundles:
    // the marked file is absent there too.
    let site_feed =
        String::from_utf8_lossy(&get(srv.port, "/rss.xml").body).into_owned();
    assert!(!site_feed.contains("secret-draft"), "{site_feed}");
    assert!(site_feed.contains("marked-bundle"), "{site_feed}");
    let source_zip = get(srv.port, "/b/marked-bundle.zip");
    assert_eq!(source_zip.status, 200);
    assert!(
        !contains_bytes(&source_zip.body, b"secret-draft"),
        "the source zip carries the marked file"
    );
    let site_zip = get(srv.port, "/b/marked-bundle/site.zip");
    assert_eq!(site_zip.status, 200);
    assert!(
        !contains_bytes(&site_zip.body, b"secret-draft"),
        "the site zip carries the marked file"
    );

    // Published-side MCP agrees: not listed, not found by search.
    let listed = tool_json(&call(srv.port, 7, "list_files", json!({"slug": slug}), None));
    let paths: Vec<&str> = listed["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert!(!paths.contains(&"secret-draft.md"), "{paths:?}");
    let search = tool_json(&call(
        srv.port,
        8,
        "search_bundle",
        json!({"slug": slug, "query": "unicorn"}),
        None,
    ));
    assert_eq!(search["hits"].as_array().unwrap().len(), 0);

    // The working copy kept the file through the publish.
    let kept = tool_json(&call(
        srv.port,
        9,
        "read_file",
        json!({"slug": slug, "path": "secret-draft.md", "draft": true, "edit_key": key}),
        None,
    ));
    assert!(kept["content"].as_str().unwrap().contains("unicorn"));

    // Un-mark and re-publish: the file joins the publication.
    let w4 = tool_json(&call(
        srv.port,
        10,
        "write_file",
        json!({"slug": slug, "path": "secret-draft.md", "content": "# Secret Draft\n\nA unicorn hides here.\n", "keep_private": false, "edit_key": key}),
        None,
    ));
    assert_eq!(w4["keep_private"], json!(false));
    let pub2 = tool_json(&call(
        srv.port,
        11,
        "publish_bundle",
        json!({"slug": slug, "edit_key": key}),
        None,
    ));
    assert_eq!(pub2["version"], json!(2));
    assert!(pub2.get("kept_private").is_none(), "{pub2}");
    let raw = get(srv.port, "/b/marked-bundle/secret-draft.md");
    assert_eq!(raw.status, 200);
    assert!(String::from_utf8_lossy(&raw.body).contains("unicorn"));

    // Mark it again and re-publish: it leaves the published tree —
    // routes, feed, and downloads all lose it at the same commit.
    let w5 = tool_json(&call(
        srv.port,
        12,
        "write_file",
        json!({"slug": slug, "path": "secret-draft.md", "content": "# Secret Draft\n\nA unicorn hides here.\n", "keep_private": true, "edit_key": key}),
        None,
    ));
    assert_eq!(w5["keep_private"], json!(true));
    let pub3 = tool_json(&call(
        srv.port,
        13,
        "publish_bundle",
        json!({"slug": slug, "edit_key": key}),
        None,
    ));
    assert_eq!(pub3["version"], json!(3));
    assert_eq!(pub3["kept_private"], json!(["secret-draft.md"]));
    assert_eq!(get(srv.port, "/b/marked-bundle/secret-draft.md").status, 404);
    assert_eq!(get(srv.port, "/b/marked-bundle/secret-draft").status, 404);
    assert_eq!(get(srv.port, "/b/marked-bundle/secret-draft.html").status, 404);
    let source_zip2 = get(srv.port, "/b/marked-bundle.zip");
    assert!(!contains_bytes(&source_zip2.body, b"secret-draft"));
    let site_zip2 = get(srv.port, "/b/marked-bundle/site.zip");
    assert!(!contains_bytes(&site_zip2.body, b"secret-draft"));
    let feed2 =
        String::from_utf8_lossy(&get(srv.port, "/b/marked-bundle/feed.xml").body).into_owned();
    assert!(!feed2.contains("secret-draft"), "{feed2}");
    let site_feed2 =
        String::from_utf8_lossy(&get(srv.port, "/rss.xml").body).into_owned();
    assert!(!site_feed2.contains("secret-draft"), "{site_feed2}");
}

/// A bundle whose every markdown file is marked has nothing to
/// publish — the refusal says so in the mark's own terms, rather
/// than failing later as an empty bundle.
#[test]
fn mcp_publish_refuses_when_everything_is_kept_private() {
    let srv = start_server();
    let w = tool_json(&call(
        srv.port,
        1,
        "write_file",
        json!({"slug": "all-private", "path": "index.md", "content": "# Only\n", "keep_private": true}),
        None,
    ));
    let key = w["edit_key"].as_str().expect("first write mints the key").to_string();
    let err = tool_error(&call(
        srv.port,
        2,
        "publish_bundle",
        json!({"slug": "all-private", "edit_key": key}),
        None,
    ));
    assert!(err.contains("keep-private"), "{err}");
}
