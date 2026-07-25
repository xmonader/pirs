//! Webhook bind address helpers.
use std::net::SocketAddr;


/// Env: set to `1`/`true` to bind webhook listeners on `0.0.0.0`.
pub const PUBLIC_BIND_ENV: &str = "PIRS_CLAW_PUBLIC_BIND";
/// Env: explicit bind host (`127.0.0.1` default, `0.0.0.0` for public).
pub const BIND_ENV: &str = "PIRS_CLAW_BIND";

/// Resolve webhook listen host. Default **localhost** (safe).
///
/// Opt-in public bind: `PIRS_CLAW_PUBLIC_BIND=1` or `PIRS_CLAW_BIND=0.0.0.0`.
pub fn webhook_bind_host() -> String {
    if let Ok(h) = std::env::var(BIND_ENV) {
        let h = h.trim();
        if !h.is_empty() {
            return h.to_string();
        }
    }
    let public = std::env::var(PUBLIC_BIND_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if public {
        "0.0.0.0".into()
    } else {
        "127.0.0.1".into()
    }
}

pub fn webhook_socket_addr(port: u16) -> SocketAddr {
    let host = webhook_bind_host();
    // Parse host:port; fall back to loopback if malformed host.
    format!("{host}:{port}")
        .parse()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], port)))
}
