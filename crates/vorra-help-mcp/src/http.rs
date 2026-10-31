use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;

use crate::server::HelpServer;

pub const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024;
const BURST: f64 = 60.0;
const REFILL_PER_SECOND: f64 = 1.0;
const MAX_TRACKED_CLIENTS: usize = 4096;
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];
const APP_SITE_ASSOCIATION: &str = include_str!("../site/apple-app-site-association");
const PAIR_PAGE: &str = include_str!("../site/pair.html");
const PAIR_SCRIPT: &str = include_str!("../site/pair.js");
const PAIR_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

async fn app_site_association() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "max-age=3600"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        APP_SITE_ASSOCIATION,
    )
}

async fn pair_page() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CONTENT_SECURITY_POLICY, PAIR_POLICY),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        PAIR_PAGE,
    )
}

async fn pair_script() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CONTENT_SECURITY_POLICY, PAIR_POLICY),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        PAIR_SCRIPT,
    )
}

pub struct HttpOptions {
    pub address: SocketAddr,
    pub allowed_hosts: Vec<String>,
    pub trust_forwarded_for: bool,
}

struct RateLimiter {
    trust_forwarded_for: bool,
    buckets: Mutex<HashMap<IpAddr, (f64, Instant)>>,
}

impl RateLimiter {
    fn admit(&self, client: IpAddr) -> bool {
        let now = Instant::now();
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let refilled = |(tokens, last): (f64, Instant)| {
            (tokens + now.duration_since(last).as_secs_f64() * REFILL_PER_SECOND).min(BURST)
        };
        if !buckets.contains_key(&client) && buckets.len() >= MAX_TRACKED_CLIENTS {
            buckets.retain(|_, bucket| refilled(*bucket) < BURST);
            if buckets.len() >= MAX_TRACKED_CLIENTS {
                return false;
            }
        }
        let bucket = buckets.entry(client).or_insert((BURST, now));
        let tokens = refilled(*bucket);
        if tokens < 1.0 {
            *bucket = (tokens, now);
            return false;
        }
        *bucket = (tokens - 1.0, now);
        true
    }

    fn client(&self, request: &Request, peer: SocketAddr) -> IpAddr {
        self.trust_forwarded_for
            .then(|| {
                request
                    .headers()
                    .get("x-forwarded-for")?
                    .to_str()
                    .ok()?
                    .rsplit(',')
                    .next()?
                    .trim()
                    .parse()
                    .ok()
            })
            .flatten()
            .unwrap_or(peer.ip())
    }
}

async fn rate_limit(
    State(limiter): State<Arc<RateLimiter>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    if limiter.admit(limiter.client(&request, peer)) {
        next.run(request).await
    } else {
        (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "1")],
            "rate limit exceeded\n",
        )
            .into_response()
    }
}

pub async fn serve(server: HelpServer, options: HttpOptions) -> io::Result<()> {
    let cancellation = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_max_request_body_bytes(MAX_REQUEST_BODY_BYTES)
        .with_allowed_hosts(
            LOOPBACK_HOSTS
                .into_iter()
                .map(str::to_owned)
                .chain(options.allowed_hosts),
        )
        .with_cancellation_token(cancellation.child_token());
    let service: StreamableHttpService<HelpServer, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(server.clone()), Arc::default(), config);
    let limiter = Arc::new(RateLimiter {
        trust_forwarded_for: options.trust_forwarded_for,
        buckets: Mutex::new(HashMap::new()),
    });
    let app = Router::new()
        .nest_service("/mcp", service)
        .route(
            "/.well-known/apple-app-site-association",
            get(app_site_association),
        )
        .route("/pair", get(pair_page))
        .route("/pair/open.js", get(pair_script))
        .layer(middleware::from_fn_with_state(limiter, rate_limit))
        .route("/healthz", get(|| async { "ok\n" }));
    let listener = tokio::net::TcpListener::bind(options.address).await?;
    eprintln!(
        "vorra-help-mcp: listening on http://{}/mcp",
        listener.local_addr()?
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancellation.cancel();
    })
    .await
}
