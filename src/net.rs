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
