use super::*;

/// Review F10: the `Host`/`Origin` values this console accepts. The server
/// has no auth and its endpoints are powerful (`POST /api/runs/:id/rerun`
/// runs arbitrary CLIs, `POST /api/flows/save` writes YAML), and it binds
/// loopback by default — a malicious local page could DNS-rebind
/// `attacker.com` to 127.0.0.1 and drive the console from the victim's
/// browser. Browsers send `Host` and `Origin` faithfully, so checking both
/// closes that hole without introducing a CORS requirement: absent `Origin`
/// (curl, CLI, same-origin navigations) keeps working; only clearly foreign
/// origins on state-changing (POST) requests are rejected.
///
/// Built once from the bound [`SocketAddr`]: a loopback bind accepts the
/// loopback names (with any port); an explicit non-loopback bind *additionally*
/// accepts exactly that address (any port). An arbitrary `Host` is never
/// accepted — in particular a `--bind 0.0.0.0` wildcard does not become a
/// wildcard Host; bind a concrete address to serve a LAN.
#[derive(Clone, Debug)]
pub(super) struct AllowedHosts {
    /// Lowercased host names, port already stripped, that may appear in
    /// `Host` (and, on POST, in `Origin`).
    names: Vec<String>,
}

const LOOPBACK_NAMES: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

impl AllowedHosts {
    /// Derive the allow-set from what the server actually bound. Loopback
    /// names are always in the set (even a LAN bind should still answer to
    /// `localhost` on the same machine); a non-loopback bind adds exactly
    /// its own address.
    pub(super) fn for_bind(bind: SocketAddr) -> Self {
        let mut names: Vec<String> = LOOPBACK_NAMES.iter().map(|s| s.to_string()).collect();
        names.push(bind.ip().to_string());
        names.sort();
        names.dedup();
        AllowedHosts { names }
    }

    /// Test a raw `Host` header value ("127.0.0.1:8787", "[::1]:9000", …).
    pub(super) fn allows_host(&self, host: &str) -> bool {
        self.names.iter().any(|n| n == &normalize_host(host))
    }

    /// Test a raw `Origin` header value ("http://127.0.0.1:8787"). Only ever
    /// called on POST; `null` (sandboxed/file contexts) is foreign and fails.
    pub(super) fn allows_origin(&self, origin: &str) -> bool {
        let Some(rest) = origin.split_once("://").map(|(_, r)| r) else {
            return false;
        };
        let authority = rest.split('/').next().unwrap_or("");
        self.names.iter().any(|n| n == &normalize_host(authority))
    }
}

/// Lowercase a host token and strip its port. Handles the bracketed IPv6
/// forms browsers actually send (`[::1]:8787`); an unbracketed multi-colon
/// value is a bare IPv6 literal and has no port to strip.
pub(super) fn normalize_host(value: &str) -> String {
    let v = value.trim();
    let (name, maybe_port) = match v.rsplit_once(':') {
        Some((n, p)) => (n, Some(p)),
        None => (v, None),
    };
    let bracketed = v.starts_with('[');
    let strip = match maybe_port {
        Some(p) => {
            (bracketed && name.ends_with(']') || !bracketed && !name.contains(':'))
                && p.parse::<u16>().is_ok()
        }
        None => false,
    };
    let name = if strip { name } else { v };
    name.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase()
}

/// Review F10: outermost layer on every route. A `Host` we did not derive
/// from the bind is a 400 (the request is not addressed to us at all —
/// typically a rebinding attempt); a foreign `Origin` on a state-changing
/// POST is a 403. Reads are never Origin-checked, so the SSE event stream
/// (`GET /api/runs/:id/events`) and normal navigation are unaffected.
pub(super) async fn boundary_guard(
    State(state): State<std::sync::Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match host {
        Some(h) if state.allowed_hosts.allows_host(&h) => {}
        Some(_) => return deny(StatusCode::BAD_REQUEST, "Host not allowed"),
        None => return deny(StatusCode::BAD_REQUEST, "missing Host header"),
    }
    if req.method() == Method::POST {
        if let Some(origin) = req.headers().get(ORIGIN).and_then(|v| v.to_str().ok()) {
            if !state.allowed_hosts.allows_origin(origin) {
                return deny(StatusCode::FORBIDDEN, "Origin not allowed");
            }
        }
    }
    next.run(req).await
}

/// Plain-text rejection with a short reason, per the review: the body must
/// say *why* (a bad Host is a misconfiguration, not a mystery 400).
fn deny(status: StatusCode, reason: &'static str) -> axum::response::Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        reason,
    )
        .into_response()
}
