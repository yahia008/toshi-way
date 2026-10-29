//! The platform as the default arbiter, over the HTTP API (mock node, in-memory store).

use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use escrow_api::{ensure_operator, router, ApiConfig, AppState};
use escrow_core::{
    mock::MockLightning,
    model::{NodeInvoiceState, Preimage},
    service::EscrowService,
    store::MemoryStore,
    timing::TimingConfig,
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const T0: i64 = 1_000_000;
const OPERATOR_TOKEN: &str = "operator-token-for-tests-0123456789";

struct App {
    router: Router,
    node: MockLightning,
    service: Arc<EscrowService<MockLightning, Arc<MemoryStore>>>,
    clock: Arc<AtomicI64>,
    store: Arc<MemoryStore>,
}

struct Account {
    id: String,
    token: String,
}

fn build(store: &Arc<MemoryStore>, service: &Arc<EscrowService<MockLightning, Arc<MemoryStore>>>, clock: &Arc<AtomicI64>, platform: Option<String>) -> Router {
    let now = {
        let clock = clock.clone();
        Arc::new(move || clock.load(Ordering::SeqCst))
    };
    let config = ApiConfig { rate_limit_per_min: 100_000, register_limit_per_min: 100_000, platform_arbiter_id: platform, ..Default::default() };
    router(AppState::new(service.clone(), store.clone(), config, now))
}

/// No platform arbiter configured.
async fn app() -> App {
    let node = MockLightning::new();
    let store = Arc::new(MemoryStore::new());
    let service = Arc::new(EscrowService::new(node.clone(), store.clone(), TimingConfig::default()));
    let clock = Arc::new(AtomicI64::new(T0));
    ensure_operator(&store, OPERATOR_TOKEN, T0).await.unwrap();
    App { router: build(&store, &service, &clock, None), node, service, clock, store }
}

/// A server whose platform arbiter is a fresh account. With `key` it has registered one.
async fn app_with_platform(key: bool) -> (App, Account) {
    let mut app = app().await;
    let platform = app.register().await;
    if key {
        let (s, v) = app.call(Method::PUT, "/api/v1/me/arbiter-key", Some(&platform.token), Some(json!({"public_key": spki(7)}))).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    app.router = build(&app.store, &app.service, &app.clock, Some(platform.id.clone()));
    (app, platform)
}

/// An uncompressed P-256 SPKI, the shape the server checks for (the point need not be real here).
fn spki(fill: u8) -> String {
    let mut der = vec![
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48,
        0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00, 0x04,
    ];
    der.extend([fill; 64]);
    STANDARD.encode(der)
}

/// Stand-in for the buyer\'s secret sealed to the arbiter: the right shape, opaque to the server.
fn sealed(tag: u8) -> String {
    STANDARD.encode([tag; 125])
}

fn secret(n: u8) -> Preimage {
    Preimage::from_bytes([n; 32])
}

// Shared with the arbitration tests, which use more of these than this file does.
#[allow(dead_code)]
impl App {
    async fn call(&self, method: Method, path: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = self.router.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn register(&self) -> Account {
        let (s, v) = self.call(Method::POST, "/api/v1/users", None, None).await;
        assert_eq!(s, StatusCode::CREATED);
        Account { id: v["id"].as_str().unwrap().into(), token: v["token"].as_str().unwrap().into() }
    }

    /// A user who has registered a key and can be named as an arbiter.
    async fn arbiter(&self, fill: u8) -> Account {
        let a = self.register().await;
        let (s, v) = self.call(Method::PUT, "/api/v1/me/arbiter-key", Some(&a.token), Some(json!({"public_key": spki(fill)}))).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        a
    }

    async fn create(&self, seller: &Account, buyer: Option<&Account>, arbiter: Option<&Account>) -> (StatusCode, Value) {
        let mut body = json!({"amount_sats": 1000, "description": "parcel", "hold_secs": 3600});
        if let Some(b) = buyer {
            body["buyer_id"] = json!(b.id);
        }
        if let Some(a) = arbiter {
            body["arbiter_id"] = json!(a.id);
        }
        self.call(Method::POST, "/api/v1/escrows", Some(&seller.token), Some(body)).await
    }

    async fn accept(&self, buyer: &Account, id: &str, p: &Preimage, sealed: Option<String>) -> (StatusCode, Value) {
        let mut body = json!({"payment_hash": hex::encode(p.hash())});
        if let Some(s) = sealed {
            body["sealed_secret"] = json!(s);
        }
        self.call(Method::POST, &format!("/api/v1/escrows/{id}/accept"), Some(&buyer.token), Some(body)).await
    }

    async fn pay(&self, p: &Preimage) {
        let ev = self.node.pay(&p.hash());
        self.service.apply_event(ev, self.clock.load(Ordering::SeqCst)).await.unwrap();
    }

    /// An arbitrated escrow, accepted with a sealed secret and locked.
    async fn locked(&self, seller: &Account, buyer: &Account, arbiter: &Account, p: &Preimage) -> String {
        let (s, e) = self.create(seller, Some(buyer), Some(arbiter)).await;
        assert_eq!(s, StatusCode::CREATED, "{e}");
        let id = e["id"].as_str().unwrap().to_string();
        assert_eq!(self.accept(buyer, &id, p, Some(sealed(1))).await.0, StatusCode::OK);
        self.pay(p).await;
        id
    }

    async fn dispute(&self, who: &Account, id: &str) {
        let (s, v) = self
            .call(Method::POST, &format!("/api/v1/escrows/{id}/dispute"), Some(&who.token), Some(json!({"reason": "not delivered"})))
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }

    async fn decide(&self, who: &Account, id: &str, body: Value) -> (StatusCode, Value) {
        self.call(Method::POST, &format!("/api/v1/escrows/{id}/arbitration/decide"), Some(&who.token), Some(body)).await
    }

    async fn state(&self, token: &str, id: &str) -> String {
        let (_, v) = self.call(Method::GET, &format!("/api/v1/escrows/{id}"), Some(token), None).await;
        v["state"].as_str().unwrap_or("?").into()
    }
}


fn code(v: &Value) -> Option<&str> {
    v["error"]["code"].as_str()
}

impl App {
    async fn create_plain(&self, seller: &Account, extra: Value) -> (StatusCode, Value) {
        let mut body = json!({"amount_sats": 1000, "description": "parcel", "hold_secs": 3600});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        self.call(Method::POST, "/api/v1/escrows", Some(&seller.token), Some(body)).await
    }
}

#[tokio::test]
async fn an_escrow_with_no_named_arbiter_gets_the_platforms_and_shows_its_key() {
    let (app, platform) = app_with_platform(true).await;
    let seller = app.register().await;

    let (s, e) = app.create_plain(&seller, json!({})).await;
    assert_eq!(s, StatusCode::CREATED, "{e}");
    assert_eq!(e["arbiter_id"], platform.id.as_str());
    assert_eq!(e["arbiter_key"], spki(7).as_str());
    assert!(e["arbiter_fingerprint"].is_string());

    // Anyone signed in can see who the platform arbiter is, to compare the fingerprint.
    let (s, d) = app.call(Method::GET, "/api/v1/arbiter/default", Some(&seller.token), None).await;
    assert_eq!((s, d["id"].as_str(), &d["fingerprint"]), (StatusCode::OK, Some(platform.id.as_str()), &e["arbiter_fingerprint"]));

    // The buyer must then seal their secret to it, like any arbitrated escrow.
    let buyer = app.register().await;
    let id = e["id"].as_str().unwrap();
    let (s, v) = app.accept(&buyer, id, &secret(1), None).await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, Some("invalid_request")), "{v}");
    assert_eq!(app.accept(&buyer, id, &secret(1), Some(sealed(1))).await.0, StatusCode::OK);
}

#[tokio::test]
async fn a_seller_can_opt_out_or_name_their_own_arbiter() {
    let (app, platform) = app_with_platform(true).await;
    let seller = app.register().await;

    let (s, e) = app.create_plain(&seller, json!({"no_arbiter": true})).await;
    assert_eq!(s, StatusCode::CREATED, "{e}");
    assert!(e["arbiter_id"].is_null() && e["arbiter_key"].is_null(), "{e}");

    let own = app.arbiter(9).await;
    let (s, e) = app.create_plain(&seller, json!({"arbiter_id": own.id})).await;
    assert_eq!((s, e["arbiter_id"].as_str()), (StatusCode::CREATED, Some(own.id.as_str())), "{e}");
    assert_ne!(own.id, platform.id);

    // Naming one and opting out is just naming one: the opt-out only declines the platform's.
    let (s, e) = app.create_plain(&seller, json!({"arbiter_id": own.id, "no_arbiter": true})).await;
    assert_eq!((s, e["arbiter_id"].as_str()), (StatusCode::CREATED, Some(own.id.as_str())), "{e}");
}

#[tokio::test]
async fn the_platform_arbiter_is_never_added_where_it_cannot_serve() {
    // Not configured: nothing is added and the lookup says so.
    let plain = app().await;
    let seller = plain.register().await;
    let (_, e) = plain.create_plain(&seller, json!({})).await;
    assert!(e["arbiter_id"].is_null(), "{e}");
    let (_, d) = plain.call(Method::GET, "/api/v1/arbiter/default", Some(&seller.token), None).await;
    assert!(d["id"].is_null(), "{d}");

    // Configured but it has not set up its key: escrows are still made, with no arbiter.
    let (keyless, _) = app_with_platform(false).await;
    let seller = keyless.register().await;
    let (s, e) = keyless.create_plain(&seller, json!({})).await;
    assert_eq!(s, StatusCode::CREATED, "{e}");
    assert!(e["arbiter_id"].is_null(), "{e}");
    let (_, d) = keyless.call(Method::GET, "/api/v1/arbiter/default", Some(&seller.token), None).await;
    assert!(d["id"].is_null(), "{d}");

    // It is a party to the escrow: it cannot judge its own.
    let (app, platform) = app_with_platform(true).await;
    let (s, e) = app.create_plain(&platform, json!({})).await;
    assert_eq!(s, StatusCode::CREATED, "{e}");
    assert!(e["arbiter_id"].is_null(), "selling as the platform arbiter: {e}");
    let seller = app.register().await;
    let (s, e) = app.create_plain(&seller, json!({"buyer_id": platform.id})).await;
    assert_eq!(s, StatusCode::CREATED, "{e}");
    assert!(e["arbiter_id"].is_null(), "buying as the platform arbiter: {e}");
}

#[tokio::test]
async fn the_platform_account_decides_a_dispute_like_any_arbiter() {
    let (app, platform) = app_with_platform(true).await;
    let (seller, buyer) = (app.register().await, app.register().await);
    let p = secret(2);
    let (s, e) = app.create_plain(&seller, json!({"buyer_id": buyer.id})).await;
    assert_eq!(s, StatusCode::CREATED, "{e}");
    let id = e["id"].as_str().unwrap().to_string();
    assert_eq!(app.accept(&buyer, &id, &p, Some(sealed(2))).await.0, StatusCode::OK);
    app.pay(&p).await;

    // The seller disputes; the platform account (and not the operator) now holds the decision.
    let (s, _) = app.call(Method::POST, &format!("/api/v1/escrows/{id}/dispute"), Some(&seller.token), Some(json!({"reason": "not released"}))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = app.call(Method::POST, &format!("/api/v1/escrows/{id}/cancel"), Some(OPERATOR_TOKEN), None).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, Some("forbidden")), "{v}");

    let body = json!({"decision": "release", "preimage": hex::encode(p.as_bytes()), "note": "proof of delivery accepted"});
    let (s, v) = app.call(Method::POST, &format!("/api/v1/escrows/{id}/arbitration/decide"), Some(&platform.token), Some(body)).await;
    assert_eq!((s, v["state"].as_str()), (StatusCode::OK, Some("SETTLED")), "{v}");
    assert_eq!(app.node.state(&p.hash()), Some(NodeInvoiceState::Settled));
}
