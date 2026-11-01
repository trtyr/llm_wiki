//! Response construction helpers + request rate limiting.
//! Extracted from api_server/mod.rs; visibility is pub(super) so the
//! parent module (server loop, router, handlers) can use them.

use serde_json::Value;
use tiny_http::Header;

use super::router::cors_headers;
use super::*;

pub(super) const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(1);
pub(super) const RATE_LIMIT_MAX_REQUESTS: usize = 120;
pub(super) static RATE_LIMIT: OnceLock<Mutex<VecDeque<Instant>>> = OnceLock::new();

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

pub(super) fn respond_error(request: tiny_http::Request, status: u16, message: &str, origin: Option<&str>) {
    respond_json(
        request,
        status,
        json!({ "ok": false, "error": message }),
        origin,
    );
}

pub(super) fn respond_options(request: tiny_http::Request, origin: Option<&str>) {
    let mut response = Response::empty(StatusCode(204));
    for header in cors_headers(origin) {
        response.add_header(header);
    }
    response.add_header(Header::from_bytes("Access-Control-Max-Age", "600").unwrap());
    let _ = request.respond(response);
}

pub(super) fn respond_json(request: tiny_http::Request, status: u16, body: Value, origin: Option<&str>) {
    let mut response = Response::from_string(body.to_string()).with_status_code(StatusCode(status));
    for header in cors_headers(origin) {
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

pub(super) fn allow_request() -> bool {
    let now = Instant::now();
    let window_start = now - RATE_LIMIT_WINDOW;
    let lock = RATE_LIMIT.get_or_init(|| Mutex::new(VecDeque::new()));
    let Ok(mut hits) = lock.lock() else {
        return false;
    };
    while hits.front().map(|t| *t < window_start).unwrap_or(false) {
        hits.pop_front();
    }
    if hits.len() >= RATE_LIMIT_MAX_REQUESTS {
        return false;
    }
    hits.push_back(now);
    true
}

