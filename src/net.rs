//! Shared HTTP agent. Uses the system CA bundle when present (respects
//! `SSL_CERT_FILE`), honours `HTTPS_PROXY`/`HTTP_PROXY`, and identifies itself
//! politely as MusicBrainz requires.

use anyhow::{Context, Result};
use std::sync::OnceLock;
use std::time::Duration;
use ureq::tls::{parse_pem, PemItem, RootCerts, TlsConfig};

pub const USER_AGENT: &str = concat!("Coda/", env!("CARGO_PKG_VERSION"), " ( Linux CD ripper and audio converter )");

fn system_roots() -> Option<RootCerts> {
    let path = std::env::var("SSL_CERT_FILE").ok().or_else(|| {
        ["/etc/ssl/certs/ca-certificates.crt", "/etc/pki/tls/certs/ca-bundle.crt", "/etc/ssl/ca-bundle.pem"]
            .iter()
            .find(|p| std::path::Path::new(p).exists())
            .map(|s| s.to_string())
    })?;
    let pem = std::fs::read(path).ok()?;
    let certs: Vec<_> = parse_pem(&pem)
        .filter_map(|i| match i {
            Ok(PemItem::Certificate(c)) => Some(c),
            _ => None,
        })
        .collect();
    (!certs.is_empty()).then(|| RootCerts::new_with_certs(&certs))
}

pub fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let mut tls = TlsConfig::builder();
        if let Some(roots) = system_roots() {
            tls = tls.root_certs(roots);
        }
        ureq::Agent::config_builder()
            .user_agent(USER_AGENT)
            .timeout_global(Some(Duration::from_secs(30)))
            .tls_config(tls.build())
            .build()
            .into()
    })
}

pub fn get_bytes(url: &str) -> Result<Vec<u8>> {
    let mut resp = agent().get(url).call().with_context(|| format!("GET {url}"))?;
    Ok(resp.body_mut().with_config().limit(64 * 1024 * 1024).read_to_vec()?)
}

pub fn get_json(url: &str) -> Result<serde_json::Value> {
    let mut resp = agent().get(url).header("Accept", "application/json").call().with_context(|| format!("GET {url}"))?;
    Ok(resp.body_mut().with_config().limit(16 * 1024 * 1024).read_json()?)
}

/// GET with an optional Authorization header (Discogs personal tokens).
pub fn get_json_with_auth(url: &str, auth: Option<&str>) -> Result<serde_json::Value> {
    let mut req = agent().get(url).header("Accept", "application/json");
    if let Some(a) = auth {
        req = req.header("Authorization", a);
    }
    let mut resp = req.call().with_context(|| format!("GET {url}"))?;
    Ok(resp.body_mut().with_config().limit(16 * 1024 * 1024).read_json()?)
}

pub fn get_bytes_with_auth(url: &str, auth: Option<&str>) -> Result<Vec<u8>> {
    let mut req = agent().get(url);
    if let Some(a) = auth {
        req = req.header("Authorization", a);
    }
    let mut resp = req.call().with_context(|| format!("GET {url}"))?;
    Ok(resp.body_mut().with_config().limit(64 * 1024 * 1024).read_to_vec()?)
}

/// HTTP status of a failed request, if that's what failed.
pub fn status_of(e: &anyhow::Error) -> Option<u16> {
    e.chain().find_map(|c| match c.downcast_ref::<ureq::Error>() {
        Some(ureq::Error::StatusCode(s)) => Some(*s),
        _ => None,
    })
}

pub fn get_text(url: &str) -> Result<String> {
    let mut resp = agent().get(url).call().with_context(|| format!("GET {url}"))?;
    Ok(resp.body_mut().with_config().limit(4 * 1024 * 1024).read_to_string()?)
}

pub fn is_not_found(e: &anyhow::Error) -> bool {
    e.chain().any(|c| matches!(c.downcast_ref::<ureq::Error>(), Some(ureq::Error::StatusCode(404))))
}

/// Minimal percent-encoding for query values.
pub fn urlencode(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            b' ' => o.push('+'),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

/// A short, human message for a failed lookup: no URLs, says what to do.
pub fn describe(service: &str, e: &anyhow::Error) -> String {
    match status_of(e) {
        Some(503) | Some(502) | Some(504) => return format!("{service} is busy or down for maintenance right now"),
        Some(429) => return format!("{service} asked us to slow down (rate limit)"),
        Some(401) | Some(403) => return format!("{service} refused the request (HTTP {})", status_of(e).unwrap_or(0)),
        Some(code) => return format!("{service} returned an error (HTTP {code})"),
        None => {}
    }
    let offline = e.chain().any(|c| {
        matches!(
            c.downcast_ref::<ureq::Error>(),
            Some(ureq::Error::Io(_) | ureq::Error::HostNotFound | ureq::Error::ConnectionFailed | ureq::Error::Timeout(_))
        )
    });
    if offline {
        return format!("Couldn't reach {service}: check your internet connection");
    }
    // our own messages (e.g. Discogs token hints) are the root cause and already readable
    let root = e.root_cause().to_string();
    let short: String = root.chars().take(140).collect();
    format!("{service}: {short}")
}

/// True for errors worth retrying after a pause (busy server, rate limit, timeout).
pub fn is_transient(e: &anyhow::Error) -> bool {
    matches!(status_of(e), Some(429 | 500 | 502 | 503 | 504))
        || e.chain().any(|c| matches!(c.downcast_ref::<ureq::Error>(), Some(ureq::Error::Timeout(_))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// One-shot local HTTP server answering with `status`.
    fn serve(status: u16) -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut c, _)) = l.accept() {
                let mut buf = [0u8; 2048];
                let _ = c.read(&mut buf);
                let _ = write!(c, "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        });
        format!("http://{addr}/ws/2/discid/abc?toc=1+15+264208&inc=artist-credits&fmt=json")
    }

    #[test]
    fn friendly_messages_have_no_urls() {
        let e = get_json(&serve(503)).unwrap_err();
        assert!(is_transient(&e));
        let m = describe("MusicBrainz", &e);
        assert_eq!(m, "MusicBrainz is busy or down for maintenance right now");
        let e = get_json(&serve(404)).unwrap_err();
        assert!(!is_transient(&e) && is_not_found(&e));
        let e = get_json("http://127.0.0.1:9/nothing-listens-here").unwrap_err();
        let m = describe("GnuDB", &e);
        assert!(m.starts_with("Couldn't reach GnuDB"), "{m}");
        assert!(!m.contains("http"));
    }
}
