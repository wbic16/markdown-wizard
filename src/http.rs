//! The HTTP layer, distilled from phext-lattice's `phext_edit.rs` and
//! `oauth.rs`: a std-only server — `TcpListener`, one thread per
//! connection, hand-written HTTP/1.1 responses with `Connection: close`,
//! strong SHA-256 ETags with `If-None-Match` revalidation, cache classes,
//! and the lattice security-header block (CSP, HSTS when the request
//! arrived over HTTPS via the proxy). No web framework: that is the
//! stack phext-edit runs in production, reduced to the parts a
//! publish-and-serve tool needs.
//!
//! One deviation from the lattice CSP, deliberate and narrow: `img-src`
//! also allows `data:` and `https:` so a published page can show images
//! the author linked. Everything else matches the lattice policy.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

use sha2::Digest;

pub const HTML: &str = "text/html; charset=utf-8";
pub const TEXT: &str = "text/plain; charset=utf-8";
pub const JSON: &str = "application/json; charset=utf-8";

/// How a response may be cached — the lattice's three classes.
#[derive(Clone, Copy, PartialEq)]
pub enum CacheClass {
    /// API responses, errors, mutable state: never stored, no ETag.
    NoStore,
    /// The editor shell and other pages that change with the binary or
    /// with a re-publish: ETag + short freshness, revalidate after.
    Shell,
    /// Compiled-in bytes that change only when the binary does.
    Static,
}

fn cache_control(class: CacheClass) -> &'static str {
    match class {
        CacheClass::NoStore => "no-store",
        CacheClass::Shell => "public, max-age=60, must-revalidate",
        CacheClass::Static => "public, max-age=86400, must-revalidate",
    }
}

/// One year, matching `deploy/nginx-md.phext.io.conf`. No
/// `includeSubDomains`: this host is `md.phext.io`, and a future sibling
/// name should not be pinned by accident. (Same reasoning as the
/// lattice's HSTS note for edit.phext.io.)
pub const HSTS_VALUE: &str = "max-age=31536000";

/// The lattice CSP, with the one documented `img-src` deviation.
pub const CONTENT_SECURITY_POLICY: &str = concat!(
    "default-src 'self'; ",
    "script-src 'self' 'unsafe-inline'; ",
    "style-src 'self' 'unsafe-inline'; ",
    "img-src 'self' data: https:; ",
    "font-src 'self'; ",
    "connect-src 'self'; ",
    "form-action 'self'; ",
    "base-uri 'self'; ",
    "frame-ancestors 'none'; ",
    "object-src 'none'; ",
    "frame-src 'none'"
);

/// True when the proxy says the browser connection is HTTPS — the
/// lattice's `forwarded_https`, verbatim in shape.
pub fn forwarded_https(headers: &HashMap<String, String>) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.split(',').next())
        .map(|value| value.trim().eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

fn security_header_block(https: bool) -> String {
    let mut csp = CONTENT_SECURITY_POLICY.to_string();
    if https {
        csp.push_str("; upgrade-insecure-requests");
    }
    let mut block = format!(
        "X-Content-Type-Options: nosniff\r\n\
         X-Frame-Options: DENY\r\n\
         Referrer-Policy: strict-origin-when-cross-origin\r\n\
         Permissions-Policy: accelerometer=(), camera=(), geolocation=(), gyroscope=(), magnetometer=(), microphone=(), payment=(), usb=()\r\n\
         Content-Security-Policy: {csp}\r\n"
    );
    if https {
        block.push_str(&format!("Strict-Transport-Security: {HSTS_VALUE}\r\n"));
    }
    block
}

/// The lattice's status-text table, extended with the codes this
/// service uses (409, 415, 422).
pub fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

/// A strong ETag: the quoted SHA-256 hex of the body (the lattice's
/// `strong_etag`, which hashes with SHA-256 the same way).
pub fn strong_etag(body: &[u8]) -> String {
    let digest = sha2::Sha256::digest(body);
    let mut hex = String::with_capacity(2 + digest.len() * 2);
    hex.push('"');
    for byte in digest {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        hex.push(HEX[(byte >> 4) as usize] as char);
        hex.push(HEX[(byte & 0xf) as usize] as char);
    }
    hex.push('"');
    hex
}

fn if_none_match(headers: &HashMap<String, String>, tag: &str) -> bool {
    headers
        .get("if-none-match")
        .map(|value| value.split(',').any(|part| part.trim() == tag || part.trim() == "*"))
        .unwrap_or(false)
}

/// Parse a request line and headers — the lattice's `parse_request`
/// shape: method, raw target, and a lower-cased header map.
pub fn parse_request(
    reader: &mut BufReader<TcpStream>,
) -> Option<(String, String, HashMap<String, String>)> {
    // The head — request line plus headers — is capped: a client
    // may not grow it without bound, by endless header lines or by
    // one endless line. The head is read through a take() of the
    // buffered reader, so the cap binds memory as well as count;
    // bytes the buffer already pulled for the body stay in the
    // buffer for read_body. Past the cap the connection is dropped
    // without an answer. (Production also sits behind nginx, which
    // caps heads itself; the binary keeps its own bound regardless.)
    const MAX_HEAD_BYTES: u64 = 64 * 1024;
    let mut head = reader.take(MAX_HEAD_BYTES);
    let mut total = 0usize;
    let mut request_line = String::new();
    if head.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
    total += request_line.len();
    if !request_line.ends_with('\n') {
        return None; // the cap, or a peer that never finished the line
    }
    let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let method = parts[0].to_string();
    let path = parts[1].to_string();
    let mut headers = HashMap::new();
    let mut terminated = false;
    loop {
        let mut line = String::new();
        if head.read_line(&mut line).ok()? == 0 {
            break;
        }
        total += line.len();
        if !line.ends_with('\n') {
            return None; // the cap cut a header line in two
        }
        let line = line.trim().to_string();
        if line.is_empty() {
            terminated = true;
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    if !terminated && total as u64 >= MAX_HEAD_BYTES {
        return None; // the cap, spent exactly at a line boundary
    }
    Some((method, path, headers))
}

/// Read a request body as bytes, up to `cap`. Returns `None` when the
/// declared length exceeds the cap (the caller answers 413). The
/// lattice reads bodies as lossy text; bundles are binary, so this
/// service reads bytes — same Content-Length discipline.
pub fn read_body(
    reader: &mut BufReader<TcpStream>,
    headers: &HashMap<String, String>,
    cap: usize,
) -> Option<Vec<u8>> {
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if len > cap {
        return None;
    }
    if len == 0 {
        return Some(Vec::new());
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// Build a full response — the lattice's `response_bytes`: ETag and
/// 304 handling for the cacheable classes, security headers on
/// everything, `Connection: close`.
pub fn response_bytes(
    status: u16,
    content_type: &str,
    body: &[u8],
    class: CacheClass,
    req_headers: &HashMap<String, String>,
    head_only: bool,
    extra_headers: &str,
) -> Vec<u8> {
    let https = forwarded_https(req_headers);
    let etag = match class {
        CacheClass::NoStore => None,
        CacheClass::Shell | CacheClass::Static => Some(strong_etag(body)),
    };
    if status == 200 {
        if let Some(ref tag) = etag {
            if if_none_match(req_headers, tag) {
                let head = format!(
                    "HTTP/1.1 304 Not Modified\r\nETag: {tag}\r\nCache-Control: {}\r\nContent-Length: 0\r\nConnection: close\r\n{}\r\n",
                    cache_control(class),
                    security_header_block(https)
                );
                return head.into_bytes();
            }
        }
    }
    let etag_line = etag.map(|tag| format!("ETag: {tag}\r\n")).unwrap_or_default();
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: {}\r\n{etag_line}{extra_headers}Connection: close\r\n{}\r\n",
        status,
        status_text(status),
        body.len(),
        cache_control(class),
        security_header_block(https)
    )
    .into_bytes();
    if !head_only {
        out.extend_from_slice(body);
    }
    out
}

pub fn send_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    class: CacheClass,
    req_headers: &HashMap<String, String>,
    head_only: bool,
) {
    send_response_extra(stream, status, content_type, body, class, req_headers, head_only, "");
}

pub fn send_response_extra(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    class: CacheClass,
    req_headers: &HashMap<String, String>,
    head_only: bool,
    extra_headers: &str,
) {
    let bytes = response_bytes(status, content_type, body, class, req_headers, head_only, extra_headers);
    let _ = stream.write_all(&bytes);
    let _ = stream.flush();
}

pub fn send_redirect(stream: &mut TcpStream, location: &str, req_headers: &HashMap<String, String>) {
    let body = format!("moved: {location}\n");
    let extra = format!("Location: {location}\r\n");
    send_response_extra(
        stream,
        301,
        TEXT,
        body.as_bytes(),
        CacheClass::NoStore,
        req_headers,
        false,
        &extra,
    );
}

/// Minimal JSON string escaping for the hand-formatted API responses
/// (the lattice formats JSON with serde; this service's responses are
/// small and fixed-shape, so they are formatted directly).
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
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

/// HTML-escape text for interpolation into the page templates.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// XML-escape text for the RSS feed (same five characters as HTML,
/// without the apostrophe form XML parsers also accept). Control
/// characters XML 1.0 forbids outright — everything below U+0020
/// except tab, line feed, and carriage return — are replaced with
/// U+FFFD: left raw, a single one (a stray bell or vertical tab in
/// a markdown source, say) makes the whole feed not well-formed,
/// and every subscriber's parser refuses it — the same silent
/// death as one unescaped `&`, by another character.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {
                out.push('\u{FFFD}')
            }
            c => out.push(c),
        }
    }
    out
}

/// Percent-decode a URL path segment string. `+` is left alone (it is
/// a literal plus in a path, not a space). Invalid sequences pass
/// through unchanged; the caller's path rules reject anything odd.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
