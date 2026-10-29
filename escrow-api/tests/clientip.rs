//! Rate limits see each real client behind a trusted reverse proxy, and a forged header gains nothing
//! (in-memory store).

use std::{
    net::SocketAddr,
    sync::{atomic::AtomicI64, Arc},
};

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
    Router,
};
use escrow_api::{clientip::TrustedProxies, ensure_operator, router, ApiConfig, AppState};
use escrow_core::{mock::MockLightning, service::EscrowService, store::MemoryStore, timing::TimingConfig};
use tower::ServiceExt;

const T0: i64 = 1_000_000;

async fn app(trusted: TrustedProxies) -> Router {
    let store = Arc::new(MemoryStore::new());
    let service = Arc::new(EscrowService::new(MockLightning::new(), store.clone(), TimingConfig::default()));
    let clock = Arc::new(AtomicI64::new(T0));
    let now = Arc::new(move || clock.load(std::sync::atomic::Ordering::SeqCst));
    ensure_operator(&store, "operator-token-for-tests-0123456789", T0).await.unwrap();
    // Two sign-ups a minute per client, so the limit is easy to reach.
    let config = ApiConfig { register_limit_per_min: 2, trusted_proxies: trusted, ..Default::default() };
    router(AppState::new(service, store, config, now))
}

/// A sign-up that arrives on a connection from `peer`, with `forwarded` as `X-Forwarded-For` if given.
async fn sign_up(app: &Router, peer: &str, forwarded: Option<&str>) -> StatusCode {
    let mut req = Request::builder().method(Method::POST).uri("/api/v1/users");
    if let Some(f) = forwarded {
        req = req.header("x-forwarded-for", f);
    }
    let mut req = req.body(Body::empty()).unwrap();
    req.extensions_mut().insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    app.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn each_client_behind_the_proxy_has_their_own_limit() {
    let app = app(TrustedProxies::loopback()).await;
    // Everyone arrives from the reverse proxy on 127.0.0.1; the header says who they really are.
    for _ in 0..2 {
        assert_eq!(sign_up(&app, "127.0.0.1:5000", Some("203.0.113.1")).await, StatusCode::CREATED);
    }
    assert_eq!(sign_up(&app, "127.0.0.1:5000", Some("203.0.113.1")).await, StatusCode::TOO_MANY_REQUESTS);
    // A different person is unaffected by that one's limit. Without this they would all share one.
    assert_eq!(sign_up(&app, "127.0.0.1:5001", Some("203.0.113.2")).await, StatusCode::CREATED);
    assert_eq!(sign_up(&app, "127.0.0.1:5002", Some("198.51.100.77, 203.0.113.3")).await, StatusCode::CREATED);
}

#[tokio::test]
async fn a_direct_visitor_cannot_dodge_the_limit_by_forging_the_header() {
    let app = app(TrustedProxies::loopback()).await;
    // Not from a trusted proxy, so the header is ignored and the connection's address is what counts.
    for n in 0..2 {
        assert_eq!(sign_up(&app, "198.51.100.9:4000", Some(&format!("10.0.0.{n}"))).await, StatusCode::CREATED);
    }
    assert_eq!(sign_up(&app, "198.51.100.9:4001", Some("10.0.0.99")).await, StatusCode::TOO_MANY_REQUESTS);

    // Forging entries on the left behind a proxy does not help either: the proxy's own entry is on the right.
    let behind = self::app(TrustedProxies::loopback()).await;
    for n in 0..2 {
        assert_eq!(sign_up(&behind, "127.0.0.1:1", Some(&format!("10.0.0.{n}, 203.0.113.50"))).await, StatusCode::CREATED);
    }
    assert_eq!(sign_up(&behind, "127.0.0.1:1", Some("10.0.0.200, 203.0.113.50")).await, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn with_no_trusted_proxy_the_header_is_never_believed() {
    let app = app(TrustedProxies::none()).await;
    for n in 0..2 {
        assert_eq!(sign_up(&app, "127.0.0.1:1", Some(&format!("203.0.113.{n}"))).await, StatusCode::CREATED);
    }
    assert_eq!(sign_up(&app, "127.0.0.1:1", Some("203.0.113.99")).await, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn by_default_nothing_is_trusted_so_a_forged_header_never_works() {
    // The out-of-the-box setting, as in development behind the web app's own proxy.
    let app = app(TrustedProxies::default()).await;
    for n in 0..2 {
        assert_eq!(sign_up(&app, "127.0.0.1:1", Some(&format!("203.0.113.{n}"))).await, StatusCode::CREATED);
    }
    assert_eq!(sign_up(&app, "127.0.0.1:2", Some("203.0.113.200")).await, StatusCode::TOO_MANY_REQUESTS);
}

