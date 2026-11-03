//! Response construction helpers + request rate limiting.
//! Extracted from api_server/mod.rs; visibility is pub(super) so the
//! parent module (server loop, router, handlers) can use them.

use serde_json::Value;
use tiny_http::Header;

use super::router::cors_headers;
use super::*;
use crate::cors::requests_private_network;

pub(super) const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(1);
pub(super) const RATE_LIMIT_MAX_REQUESTS: usize = 120;
pub(super) static RATE_LIMIT: OnceLock<Mutex<HashMap<std::net::IpAddr, VecDeque<Instant>>>> =
    OnceLock::new();

pub(super) struct ApiResponse {
    pub(super) status: u16,
    pub(super) body: Value,
}

pub(super) fn ok(body: Value) -> ApiResponse {
    ApiResponse { status: 200, body }
}

pub(super) fn err(status: u16, message: impl Into<String>) -> ApiResponse {
    ApiResponse {
        status,
        body: json!({ "ok": false, "error": message.into() }),
    }
}

/// 500 responses log the internal detail server-side but return a generic
/// message to the client, so filesystem paths and storage internals are not
/// disclosed over the wire.
pub(super) fn err_internal(label: &str, error: impl std::fmt::Display) -> ApiResponse {
    eprintln!("[api] 500 {label}: {error}");
    err(500, "Internal API server error")
}

pub(super) fn respond_error(
    request: tiny_http::Request,
    status: u16,
    message: &str,
    origin: Option<&str>,
) {
    respond_json(
        request,
        status,
        json!({ "ok": false, "error": message }),
        origin,
    );
}

pub(super) fn respond_options(request: tiny_http::Request, origin: Option<&str>) {
    let mut response = Response::empty(StatusCode(204));
    for header in cors_headers(origin, requests_private_network(&request)) {
        response.add_header(header);
    }
    response.add_header(Header::from_bytes("Access-Control-Max-Age", "600").unwrap());
    let _ = request.respond(response);
}

pub(super) fn respond_json(
    request: tiny_http::Request,
    status: u16,
    body: Value,
    origin: Option<&str>,
) {
    let mut response = Response::from_string(body.to_string()).with_status_code(StatusCode(status));
    for header in cors_headers(origin, requests_private_network(&request)) {
        response.add_header(header);
    }
    let _ = request.respond(response);
}

pub(super) fn should_rate_limit(method: &Method, url: &str) -> bool {
    if method == &Method::Options {
        return false;
    }
    let (path, _) = split_url(url);
    !(path == "/health" || path == format!("{API_PREFIX}/health"))
}

pub(super) fn allow_request(peer: Option<std::net::IpAddr>) -> bool {
    let now = Instant::now();
    let window_start = now - RATE_LIMIT_WINDOW;
    let lock = RATE_LIMIT
        .get_or_init(|| Mutex::new(HashMap::<std::net::IpAddr, VecDeque<Instant>>::new()));
    let Ok(mut buckets) = lock.lock() else {
        return false;
    };
    // Per-peer buckets (F-09): a loopback-only deployment keeps exactly
    // one bucket (same as the old global one), but with LAN access on, a
    // single client can no longer exhaust everyone else's quota. The
    // map is bounded so a spoofed-source flood cannot grow it forever.
    if buckets.len() > 1024 {
        buckets.retain(|_, hits| hits.back().map(|t| *t >= window_start).unwrap_or(false));
    }
    let key = peer.unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    let hits = buckets.entry(key).or_default();
    while hits.front().map(|t| *t < window_start).unwrap_or(false) {
        hits.pop_front();
    }
    if hits.len() >= RATE_LIMIT_MAX_REQUESTS {
        return false;
    }
    hits.push_back(now);
    true
}
