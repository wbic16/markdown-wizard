//! Federation end-to-end, against a local stub of edit.phext.io.
//!
//! The stub implements the provider's OAuth + MCP surface as
//! measured in phext-lattice's src/oauth.rs and
//! src/bin/phext_edit.rs: discovery documents, dynamic client
//! registration, the authorization-code redirect (auto-consented
//! here — consent is the provider's own page, invisible to a
//! client beyond the redirect back), the token endpoint with
//! rotating refresh tokens, and the MCP tools list_phexts /
//! read_scroll / write_scroll / phext_toc behind a bearer token.
//!
//! The flow proven here:
//!   sign-in completes (DCR → authorize → code → token, our
//!   session cookie only) → a vault is named, its starter file
//!   served, and the phext "hosted" (the stub seeds it, the way a
//!   user uploading the starter would) → a draft push lands in
//!   the stub vault under the user's token → a second "browser"
//!   (fresh session, same user) discovers and pulls the draft →
//!   a both-changed file conflicts instead of overwriting, in
//!   both directions, until a named resolution → sign-out returns
//!   the endpoints to local-only → an unhosted vault reports its
//!   first-class not-hosted state.
//!
//! One simplification, stated plainly: the stub checks that a
//! PKCE verifier is presented and bound to the grant, but does
//! not hash it — the S256 transform itself is pinned by the
//! RFC 7636 appendix-B vector in fed.rs's unit tests.
//!
//! No real edit.phext.io, no production accounts: both servers
//! are spawned on loopback with throwaway state.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ── tiny HTTP plumbing (the house style: raw requests) ───────

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn http_raw(port: u16, request: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = stream.read_to_string(&mut out);
    out
}

/// One request, no redirect following. Returns (status, headers
/// lower-cased by name, body).
fn http(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> (u16, HashMap<String, String>, String) {
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n"
    );
    if let Some(b) = body {
        req += &format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        );
    }
    for (k, v) in headers {
        req += &format!("{k}: {v}\r\n");
    }
    req += "\r\n";
    if let Some(b) = body {
        req += b;
    }
    let raw = http_raw(port, &req);
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut map = HashMap::new();
    for line in head.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            map.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    (status, map, body.to_string())
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let h = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(h, 16).unwrap_or(0));
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_params(url: &str) -> HashMap<String, String> {
    let q = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    q.split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (url_decode(k), url_decode(v)))
        .collect()
}

/// Minimal JSON string-field extraction — enough for the flat
/// documents both servers exchange in this test.
fn json_str(body: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let i = body.find(&pat)?;
    let rest = body[i + pat.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    let mut out = String::new();
    let mut chars = rest[1..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => {
                let n = chars.next()?;
                out.push(match n {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    _ => n,
                });
            }
            _ => out.push(c),
        }
    }
    None
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// ── the stub provider ─────────────────────────────────────────

#[derive(Default)]
struct Stub {
    clients: HashMap<String, Vec<String>>, // client_id -> redirect_uris
    codes: HashMap<String, (String, String, String)>, // code -> (client, redirect, challenge)
    access: HashMap<String, String>,       // access token -> refresh token
    refresh: HashMap<String, String>,      // refresh token -> access token
    counter: u64,
    hosted: Vec<String>,                   // phext names, no suffix
    scrolls: HashMap<(String, String), String>, // (phext, coord) -> content
}

impl Stub {
    fn mint_pair(&mut self) -> (String, String) {
        self.counter += 1;
        let access = format!("stub-access-{}", self.counter);
        let refresh = format!("stub-refresh-{}", self.counter);
        self.access.insert(access.clone(), refresh.clone());
        self.refresh.insert(refresh.clone(), access.clone());
        (access, refresh)
    }
}

fn stub_respond(stream: &mut TcpStream, status: u16, ctype: &str, body: &str, extra: &str) {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Status",
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

fn stub_handle(mut stream: TcpStream, state: Arc<Mutex<Stub>>, issuer: String) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let head_end = loop {
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
                if raw.len() > 1_000_000 {
                    return;
                }
            }
            Err(_) => return,
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("").to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut content_length = 0usize;
    let mut authorization = String::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            match k.trim().to_lowercase().as_str() {
                "content-length" => content_length = v.trim().parse().unwrap_or(0),
                "authorization" => authorization = v.trim().to_string(),
                _ => {}
            }
        }
    }
    let mut body = raw[head_end..].to_vec();
    while body.len() < content_length {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    body.truncate(content_length);
    let body = String::from_utf8_lossy(&body).to_string();
    let path = target.split('?').next().unwrap_or("/").to_string();
    let mut st = state.lock().unwrap();

    match (method.as_str(), path.as_str()) {
        ("GET", "/.well-known/oauth-authorization-server") => {
            let doc = format!(
                "{{\"issuer\":\"{issuer}\",\"authorization_endpoint\":\"{issuer}/oauth/authorize\",\"token_endpoint\":\"{issuer}/oauth/token\",\"registration_endpoint\":\"{issuer}/oauth/register\",\"grant_types_supported\":[\"authorization_code\",\"refresh_token\"],\"response_types_supported\":[\"code\"],\"code_challenge_methods_supported\":[\"S256\"],\"token_endpoint_auth_methods_supported\":[\"none\",\"client_secret_basic\",\"client_secret_post\",\"private_key_jwt\"],\"scopes_supported\":[\"phext:read\",\"phext:write\"]}}"
            );
            stub_respond(&mut stream, 200, "application/json", &doc, "");
        }
        ("GET", "/.well-known/oauth-protected-resource") => {
            let doc = format!(
                "{{\"resource\":\"{issuer}/mcp\",\"authorization_servers\":[\"{issuer}\"],\"scopes_supported\":[\"phext:read\",\"phext:write\"],\"bearer_methods_supported\":[\"header\"]}}"
            );
            stub_respond(&mut stream, 200, "application/json", &doc, "");
        }
        ("POST", "/oauth/register") => {
            // redirect_uris: collect the quoted strings of the array.
            let mut uris = Vec::new();
            if let Some(start) = body.find("\"redirect_uris\"") {
                let rest = &body[start..];
                if let Some(open) = rest.find('[') {
                    if let Some(close) = rest.find(']') {
                        let inner = &rest[open + 1..close];
                        let mut cur = String::new();
                        let mut in_str = false;
                        let mut esc = false;
                        for c in inner.chars() {
                            if esc {
                                cur.push(c);
                                esc = false;
                            } else if c == '\\' {
                                esc = true;
                            } else if c == '"' {
                                if in_str {
                                    uris.push(std::mem::take(&mut cur));
                                }
                                in_str = !in_str;
                            } else if in_str {
                                cur.push(c);
                            }
                        }
                    }
                }
            }
            st.counter += 1;
            let client_id = format!("stub-client-{}", st.counter);
            st.clients.insert(client_id.clone(), uris);
            let doc = format!(
                "{{\"client_id\":\"{client_id}\",\"token_endpoint_auth_method\":\"none\"}}"
            );
            stub_respond(&mut stream, 201, "application/json", &doc, "");
        }
        ("GET", "/oauth/authorize") => {
            let q = query_params(&target);
            let client_id = q.get("client_id").cloned().unwrap_or_default();
            let redirect_uri = q.get("redirect_uri").cloned().unwrap_or_default();
            let challenge = q.get("code_challenge").cloned().unwrap_or_default();
            let ok = q.get("response_type").map(|v| v == "code").unwrap_or(false)
                && !challenge.is_empty()
                && q.get("code_challenge_method").map(|v| v == "S256").unwrap_or(false)
                && st
                    .clients
                    .get(&client_id)
                    .map(|uris| uris.iter().any(|u| u == &redirect_uri))
                    .unwrap_or(false);
            if !ok {
                stub_respond(
                    &mut stream,
                    400,
                    "application/json",
                    "{\"error\":\"invalid_request\"}",
                    "",
                );
                return;
            }
            st.counter += 1;
            let code = format!("stub-code-{}", st.counter);
            st.codes
                .insert(code.clone(), (client_id, redirect_uri.clone(), challenge));
            let state = q.get("state").cloned().unwrap_or_default();
            let location = format!(
                "{redirect_uri}?code={code}&state={state}&iss={}",
                json_escape(&issuer)
            );
            stub_respond(
                &mut stream,
                302,
                "text/plain",
                "consented\n",
                &format!("Location: {location}\r\n"),
            );
        }
        ("POST", "/oauth/token") => {
            let form = query_params(&format!("x?{body}"));
            let grant = form.get("grant_type").cloned().unwrap_or_default();
            let client_id = form.get("client_id").cloned().unwrap_or_default();
            let pair = match grant.as_str() {
                "authorization_code" => {
                    let code = form.get("code").cloned().unwrap_or_default();
                    let Some((c_client, c_redirect, _challenge)) = st.codes.remove(&code)
                    else {
                        stub_respond(&mut stream, 400, "application/json", "{\"error\":\"invalid_grant\"}", "");
                        return;
                    };
                    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
                    if c_client != client_id
                        || c_redirect != form.get("redirect_uri").cloned().unwrap_or_default()
                        || verifier.is_empty()
                    {
                        stub_respond(&mut stream, 400, "application/json", "{\"error\":\"invalid_grant\"}", "");
                        return;
                    }
                    Some(st.mint_pair())
                }
                "refresh_token" => {
                    let token = form.get("refresh_token").cloned().unwrap_or_default();
                    if let Some(old_access) = st.refresh.remove(&token) {
                        st.access.remove(&old_access);
                        Some(st.mint_pair())
                    } else {
                        stub_respond(&mut stream, 400, "application/json", "{\"error\":\"invalid_grant\"}", "");
                        return;
                    }
                }
                _ => None,
            };
            match pair {
                Some((access, refresh)) => {
                    let doc = format!(
                        "{{\"access_token\":\"{access}\",\"token_type\":\"Bearer\",\"expires_in\":3600,\"scope\":\"phext:read phext:write\",\"refresh_token\":\"{refresh}\"}}"
                    );
                    stub_respond(&mut stream, 200, "application/json", &doc, "");
                }
                None => stub_respond(
                    &mut stream,
                    400,
                    "application/json",
                    "{\"error\":\"unsupported_grant_type\"}",
                    "",
                ),
            }
        }
        ("POST", "/mcp") => {
            let token = authorization.strip_prefix("Bearer ").unwrap_or("");
            if !st.access.contains_key(token) {
                stub_respond(
                    &mut stream,
                    401,
                    "application/json",
                    "{\"error\":\"invalid_token\"}",
                    &format!(
                        "WWW-Authenticate: Bearer resource_metadata=\"{issuer}/.well-known/oauth-protected-resource\"\r\n"
                    ),
                );
                return;
            }
            let tool = json_str(&body, "name").unwrap_or_default();
            let (is_error, text) = stub_tool(&mut st, &tool, &body);
            let doc = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}],\"isError\":{}}}}}",
                json_escape(&text),
                is_error
            );
            stub_respond(&mut stream, 200, "application/json", &doc, "");
        }
        _ => stub_respond(
            &mut stream,
            404,
            "application/json",
            "{\"error\":\"not_found\"}",
            "",
        ),
    }
}

fn stub_tool(st: &mut Stub, tool: &str, body: &str) -> (bool, String) {
    let phext = json_str(body, "phext")
        .unwrap_or_default()
        .trim_end_matches(".phext")
        .to_string();
    match tool {
        "list_phexts" => {
            let items: Vec<String> = st
                .hosted
                .iter()
                .map(|name| {
                    let bytes: usize = st
                        .scrolls
                        .iter()
                        .filter(|((p, _), _)| p == name)
                        .map(|(_, c)| c.len())
                        .sum();
                    format!("{{\"name\":\"{name}.phext\",\"bytes\":{bytes}}}")
                })
                .collect();
            (false, format!("{{\"phexts\":[{}]}}", items.join(",")))
        }
        "read_scroll" => {
            if !st.hosted.iter().any(|n| n == &phext) {
                return (true, format!("{{\"error\":\"no such phext hosted here: {phext}\"}}"));
            }
            let coord = json_str(body, "coordinate").unwrap_or_default();
            match st.scrolls.get(&(phext.clone(), coord.clone())) {
                Some(content) => (
                    false,
                    format!(
                        "{{\"phext\":\"{phext}\",\"coordinate\":\"{coord}\",\"exists\":true,\"content\":\"{}\"}}",
                        json_escape(content)
                    ),
                ),
                None => (
                    false,
                    format!(
                        "{{\"phext\":\"{phext}\",\"coordinate\":\"{coord}\",\"exists\":false,\"content\":\"\"}}"
                    ),
                ),
            }
        }
        "write_scroll" => {
            if !st.hosted.iter().any(|n| n == &phext) {
                return (true, format!("{{\"error\":\"no such phext hosted here: {phext}\"}}"));
            }
            let coord = json_str(body, "coordinate").unwrap_or_default();
            let content = json_str(body, "content").unwrap_or_default();
            st.scrolls.insert((phext.clone(), coord.clone()), content);
            (false, format!("{{\"ok\":true,\"phext\":\"{phext}\",\"coordinate\":\"{coord}\"}}"))
        }
        "phext_toc" => {
            let mut coords: Vec<&String> = st
                .scrolls
                .keys()
                .filter(|(p, _)| p == &phext)
                .map(|(_, c)| c)
                .collect();
            coords.sort();
            let items: Vec<String> =
                coords.iter().map(|c| format!("\"{c}\"")).collect();
            (false, format!("{{\"coordinates\":[{}]}}", items.join(",")))
        }
        _ => (true, format!("{{\"error\":\"unknown tool: {tool}\"}}")),
    }
}

fn start_stub() -> (u16, Arc<Mutex<Stub>>) {
    let port = free_port();
    let state = Arc::new(Mutex::new(Stub::default()));
    let issuer = format!("http://127.0.0.1:{port}");
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    let shared = state.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(stream) = stream {
                let s = shared.clone();
                let i = issuer.clone();
                std::thread::spawn(move || stub_handle(stream, s, i));
            }
        }
    });
    (port, state)
}

// ── the server under test ─────────────────────────────────────

struct Server {
    child: Child,
    port: u16,
    dir: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start_server(issuer: Option<&str>, federation_off: bool) -> Server {
    let port = free_port();
    let dir = std::env::temp_dir().join(format!(
        "mdwiz-fed-e2e-{}-{port}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_markdown-wizard"));
    cmd.arg("--port")
        .arg(port.to_string())
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--dir")
        .arg(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(issuer) = issuer {
        cmd.env("MDWIZ_PHEXT_ISSUER", issuer);
    }
    if federation_off {
        cmd.env("MDWIZ_FEDERATION", "off");
    }
    let mut child = cmd.spawn().expect("spawn markdown-wizard");
    let mut out = child.stdout.take().unwrap();
    let mut banner = String::new();
    let mut byte = [0u8; 1];
    let deadline = Instant::now() + Duration::from_secs(15);
    while !banner.contains("listening") {
        assert!(Instant::now() < deadline, "server never announced itself");
        match out.read(&mut byte) {
            Ok(1) => banner.push(byte[0] as char),
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    Server { child, port, dir }
}

/// The full sign-in, driven the way a browser would be: our
/// login redirect, the provider's authorize redirect, our
/// callback. Returns the session cookie pair.
fn sign_in(mport: u16, sport: u16, issuer: &str) -> String {
    let (status, headers, _) = http(mport, "GET", "/auth/login", &[], None);
    assert_eq!(status, 302, "login should redirect to the provider");
    let location = headers.get("location").expect("login location").clone();
    assert!(
        location.starts_with(&format!("{issuer}/oauth/authorize?")),
        "login must aim at the provider's authorize endpoint: {location}"
    );
    let q = query_params(&location);
    assert_eq!(q.get("response_type").map(|s| s.as_str()), Some("code"));
    assert_eq!(
        q.get("code_challenge_method").map(|s| s.as_str()),
        Some("S256")
    );
    assert_eq!(
        q.get("code_challenge").map(|s| s.len()),
        Some(43),
        "the PKCE challenge is a base64url SHA-256"
    );
    assert_eq!(
        q.get("scope").map(|s| s.as_str()),
        Some("phext:read phext:write")
    );
    let redirect_uri = q.get("redirect_uri").expect("redirect_uri").clone();
    assert_eq!(
        redirect_uri,
        format!("http://127.0.0.1:{mport}/auth/callback")
    );
    // The provider (auto-consent in the double) sends the browser back.
    let path = location[issuer.len()..].to_string();
    let (status, headers, _) = http(sport, "GET", &path, &[], None);
    assert_eq!(status, 302, "authorize should redirect back");
    let back = headers.get("location").expect("callback location").clone();
    assert!(back.starts_with(&redirect_uri), "callback target: {back}");
    let bq = query_params(&back);
    assert!(bq.contains_key("code"), "the callback carries a code");
    assert_eq!(bq.get("state"), q.get("state"), "state survives the round trip");
    // Our callback: exchange, session cookie, home.
    let callback = format!("/auth/callback{}", &back[redirect_uri.len()..]);
    let (status, headers, _) = http(mport, "GET", &callback, &[], None);
    assert_eq!(status, 302, "callback should land us home");
    assert_eq!(headers.get("location").map(|s| s.as_str()), Some("/?signed-in=1"));
    let cookie = headers.get("set-cookie").expect("session cookie").clone();
    assert!(cookie.contains("HttpOnly"), "the session cookie is HttpOnly: {cookie}");
    cookie.split(';').next().unwrap().to_string()
}

fn cookie_header(cookie: &str) -> [(&str, &str); 1] {
    [("Cookie", Box::leak(cookie.to_string().into_boxed_str()) as &str)]
}

// ── the test ──────────────────────────────────────────────────

#[test]
fn federation_against_a_stub_provider() {
    let (sport, stub) = start_stub();
    let issuer = format!("http://127.0.0.1:{sport}");
    let server = start_server(Some(&issuer), false);
    let mport = server.port;

    // Signed out: the boot question answers honestly, the vault
    // endpoints refuse, and the editor carries the affordance.
    let (status, _, body) = http(mport, "GET", "/api/me", &[], None);
    assert_eq!(status, 200);
    assert!(body.contains("\"federation\":true"), "{body}");
    assert!(body.contains("\"signed_in\":false"), "{body}");
    let (status, _, _) = http(mport, "GET", "/api/vault", &[], None);
    assert_eq!(status, 401, "the vault is behind the session");
    let (status, _, body) = http(mport, "GET", "/", &[], None);
    assert_eq!(status, 200);
    assert!(body.contains("id=\"accountBtn\""), "the sign-in affordance");
    assert!(body.contains("Sign in with edit.phext.io"));
    assert!(body.contains("Your vault on edit.phext.io"));
    assert!(body.contains("id=\"vaultWord\""), "the quiet vault state");

    // ── browser one signs in ──
    let cookie1 = sign_in(mport, sport, &issuer);
    let auth1 = cookie_header(&cookie1);
    let (status, _, body) = http(mport, "GET", "/api/me", &auth1, None);
    assert_eq!(status, 200);
    assert!(body.contains("\"signed_in\":true"), "{body}");

    // A vault is named; its starter file is the manifest itself.
    let (status, _, body) = http(
        mport,
        "POST",
        "/api/vault/new",
        &auth1,
        Some("{\"label\":\"Test vault\"}"),
    );
    assert_eq!(status, 200, "{body}");
    let vault_name = json_str(&body, "name").expect("vault name");
    assert!(vault_name.starts_with("mdwiz-vault-"), "{vault_name}");
    let (status, _, starter) = http(mport, "GET", "/api/vault/starter", &auth1, None);
    assert_eq!(status, 200);
    assert!(starter.contains("markdown-wizard vault v1"), "{starter}");
    assert!(starter.contains(&vault_name), "{starter}");

    // Before hosting, sync reports the first-class not-hosted state.
    let (status, _, body) = http(
        mport,
        "POST",
        "/api/vault/sync",
        &auth1,
        Some("{\"bundle\":\"garden\",\"files\":[],\"deleted\":[]}"),
    );
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("vault_not_hosted"), "{body}");

    // The user hosts the starter (the stub seeds it, as an upload
    // would), then chooses it.
    stub.lock().unwrap().hosted.push(vault_name.clone());
    let (status, _, body) = http(
        mport,
        "POST",
        "/api/vault/choose",
        &auth1,
        Some(&format!("{{\"name\":\"{vault_name}\"}}")),
    );
    assert_eq!(status, 200, "{body}");

    // ── a draft push lands in the vault, under this identity ──
    let text1 = "# Garden\n\nFirst draft.\n";
    let push1 = format!(
        "{{\"bundle\":\"garden\",\"files\":[{{\"path\":\"index.md\",\"hash\":\"local-1\",\"base_rev\":0,\"base_hash\":\"\",\"text\":\"{}\"}}],\"deleted\":[]}}",
        json_escape(text1)
    );
    let (status, _, body) = http(mport, "POST", "/api/vault/sync", &auth1, Some(&push1));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"written\":[\"index.md\"]"), "{body}");
    let hash1 = {
        let st = stub.lock().unwrap();
        assert_eq!(
            st.scrolls
                .get(&(vault_name.clone(), "2.1.1/1.1.1/1.1.1".to_string()))
                .map(|s| s.as_str()),
            Some(text1),
            "the file scroll holds the draft"
        );
        let vault_manifest = st
            .scrolls
            .get(&(vault_name.clone(), "1.1.1/1.1.1/1.1.1".to_string()))
            .expect("the vault manifest was written")
            .clone();
        assert!(vault_manifest.contains("bundle: garden n=1"), "{vault_manifest}");
        let bundle_manifest = st
            .scrolls
            .get(&(vault_name.clone(), "1.2.1/1.1.1/1.1.1".to_string()))
            .expect("the bundle manifest was written")
            .clone();
        assert!(bundle_manifest.contains("file: 1 rev=1"), "{bundle_manifest}");
        bundle_manifest
            .split("sha256=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .expect("the manifest records the hash")
            .to_string()
    };
    assert_eq!(hash1.len(), 64, "the server computed the real SHA-256");

    // ── browser two (fresh session, same user) pulls it down ──
    let cookie2 = sign_in(mport, sport, &issuer);
    let auth2 = cookie_header(&cookie2);
    let (status, _, body) = http(mport, "GET", "/api/vault", &auth2, None);
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(&vault_name),
        "the second browser discovers the vault: {body}"
    );
    let (status, _, _) = http(
        mport,
        "POST",
        "/api/vault/choose",
        &auth2,
        Some(&format!("{{\"name\":\"{vault_name}\"}}")),
    );
    assert_eq!(status, 200);
    let (status, _, body) = http(
        mport,
        "POST",
        "/api/vault/pull",
        &auth2,
        Some("{\"bundle\":\"garden\",\"local\":[],\"deleted\":[]}"),
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"exists\":true"), "{body}");
    assert!(
        body.contains(&json_escape(text1)),
        "the draft travelled to the second browser: {body}"
    );

    // ── both change the same file: conflict, never overwrite ──
    let text2 = "# Garden\n\nSecond browser's edit.\n";
    let push2 = format!(
        "{{\"bundle\":\"garden\",\"files\":[{{\"path\":\"index.md\",\"hash\":\"local-2\",\"base_rev\":1,\"base_hash\":\"{hash1}\",\"text\":\"{}\"}}],\"deleted\":[]}}",
        json_escape(text2)
    );
    let (status, _, body) = http(mport, "POST", "/api/vault/sync", &auth2, Some(&push2));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"written\":[\"index.md\"]"), "{body}");
    let hash2 = json_str(&body, "hash").expect("new hash").to_string();

    let text3 = "# Garden\n\nFirst browser's competing edit.\n";
    let push3 = format!(
        "{{\"bundle\":\"garden\",\"files\":[{{\"path\":\"index.md\",\"hash\":\"local-3\",\"base_rev\":1,\"base_hash\":\"{hash1}\",\"text\":\"{}\"}}],\"deleted\":[]}}",
        json_escape(text3)
    );
    let (status, _, body) = http(mport, "POST", "/api/vault/sync", &auth1, Some(&push3));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"conflicts\""), "{body}");
    assert!(body.contains("index.md"), "{body}");
    {
        let st = stub.lock().unwrap();
        assert_eq!(
            st.scrolls
                .get(&(vault_name.clone(), "2.1.1/1.1.1/1.1.1".to_string()))
                .map(|s| s.as_str()),
            Some(text2),
            "a conflict never overwrites the vault copy"
        );
    }
    // Keep mine: the named force resolves it.
    let push3f = format!(
        "{{\"bundle\":\"garden\",\"files\":[{{\"path\":\"index.md\",\"hash\":\"local-3\",\"base_rev\":1,\"base_hash\":\"{hash1}\",\"text\":\"{}\"}}],\"deleted\":[],\"force\":[\"index.md\"]}}",
        json_escape(text3)
    );
    let (status, _, body) = http(mport, "POST", "/api/vault/sync", &auth1, Some(&push3f));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"written\":[\"index.md\"]"), "{body}");
    {
        let st = stub.lock().unwrap();
        assert_eq!(
            st.scrolls
                .get(&(vault_name.clone(), "2.1.1/1.1.1/1.1.1".to_string()))
                .map(|s| s.as_str()),
            Some(text3),
            "keep-mine lands the forced write"
        );
    }
    // Take the vault's: browser two, locally ahead and stale,
    // conflicts on pull and then takes.
    let pull_conflict = format!(
        "{{\"bundle\":\"garden\",\"local\":[{{\"path\":\"index.md\",\"hash\":\"local-4\",\"base_rev\":2,\"base_hash\":\"{hash2}\"}}],\"deleted\":[]}}"
    );
    let (status, _, body) = http(mport, "POST", "/api/vault/pull", &auth2, Some(&pull_conflict));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"conflicts\""), "{body}");
    let pull_take = format!(
        "{{\"bundle\":\"garden\",\"local\":[{{\"path\":\"index.md\",\"hash\":\"local-4\",\"base_rev\":2,\"base_hash\":\"{hash2}\"}}],\"deleted\":[],\"take\":[\"index.md\"]}}"
    );
    let (status, _, body) = http(mport, "POST", "/api/vault/pull", &auth2, Some(&pull_take));
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(&json_escape(text3)),
        "take-the-vault's delivers the vault text: {body}"
    );

    // ── sign-out: local-only again ──
    let (status, _, _) = http(mport, "POST", "/auth/logout", &auth1, None);
    assert_eq!(status, 200);
    let (status, _, body) = http(mport, "GET", "/api/me", &auth1, None);
    assert_eq!(status, 200);
    assert!(body.contains("\"signed_in\":false"), "{body}");
    let (status, _, _) = http(mport, "GET", "/api/vault", &auth1, None);
    assert_eq!(status, 401, "a signed-out session holds no vault");
    // The local product never noticed any of this.
    let (status, _, body) = http(mport, "GET", "/api/status", &[], None);
    assert_eq!(status, 200);
    assert!(body.contains("\"ok\":true"), "{body}");
}

#[test]
fn federation_off_is_a_clean_local_only_deployment() {
    let server = start_server(None, true);
    let mport = server.port;
    let (status, _, body) = http(mport, "GET", "/api/me", &[], None);
    assert_eq!(status, 200);
    assert!(body.contains("\"federation\":false"), "{body}");
    assert!(body.contains("\"signed_in\":false"), "{body}");
    let (status, _, _) = http(mport, "GET", "/auth/login", &[], None);
    assert_eq!(status, 404, "no door when federation is off");
}
