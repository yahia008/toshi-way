//! Arbitration over the HTTP API (mock node, in-memory store).

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
}

struct Account {
    id: String,
    token: String,
}

async fn app() -> App {
    let node = MockLightning::new();
    let store = Arc::new(MemoryStore::new());
    let service = Arc::new(EscrowService::new(node.clone(), store.clone(), TimingConfig::default()));
    let clock = Arc::new(AtomicI64::new(T0));
    let now = {
        let clock = clock.clone();
        Arc::new(move || clock.load(Ordering::SeqCst))
    };
    ensure_operator(&store, OPERATOR_TOKEN, T0).await.unwrap();
    let config = ApiConfig { rate_limit_per_min: 100_000, register_limit_per_min: 100_000, ..Default::default() };
    App { router: router(AppState::new(service.clone(), store, config, now)), node, service, clock }
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

#[tokio::test]
async fn an_arbiter_registers_a_key_and_bad_keys_are_refused() {
    let app = app().await;
    let a = app.register().await;
    let (_, v) = app.call(Method::GET, "/api/v1/me/arbiter-key", Some(&a.token), None).await;
    assert!(v["public_key"].is_null());

    let (s, v) = app.call(Method::PUT, "/api/v1/me/arbiter-key", Some(&a.token), Some(json!({"public_key": spki(5)}))).await;
    assert_eq!(s, StatusCode::OK);
    let fingerprint = v["fingerprint"].as_str().unwrap().to_string();
    assert_eq!(fingerprint.split(' ').count(), 16);
    let (_, v) = app.call(Method::GET, "/api/v1/me/arbiter-key", Some(&a.token), None).await;
    assert_eq!(v["fingerprint"], fingerprint.as_str());

    for bad in ["", "not base64!!", &STANDARD.encode([1u8; 91]), &STANDARD.encode([0u8; 10])] {
        let (s, v) = app.call(Method::PUT, "/api/v1/me/arbiter-key", Some(&a.token), Some(json!({"public_key": bad}))).await;
        assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid_key")), "{bad:.20}");
    }
    let (s, _) = app.call(Method::PUT, "/api/v1/me/arbiter-key", Some(OPERATOR_TOKEN), Some(json!({"public_key": spki(5)}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn only_a_real_other_user_with_a_key_can_be_named_arbiter() {
    let app = app().await;
    let (seller, buyer, nokey) = (app.register().await, app.register().await, app.register().await);
    let judge = app.arbiter(5).await;

    let (s, v) = app.create(&seller, Some(&buyer), Some(&nokey)).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some("arbiter_has_no_key")), "{v}");
    let ghost = Account { id: "u_nobody".into(), token: String::new() };
    let (s, v) = app.create(&seller, Some(&buyer), Some(&ghost)).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some("unknown_arbiter")));
    for same in [&seller, &buyer] {
        let (s, v) = app.create(&seller, Some(&buyer), Some(same)).await;
        assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid_arbiter")), "{v}");
    }

    let (s, v) = app.create(&seller, Some(&buyer), Some(&judge)).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["arbiter_id"], judge.id.as_str());
    assert_eq!(v["arbiter_key"], spki(5).as_str());
    assert!(v["arbiter_fingerprint"].as_str().is_some());
    assert_eq!(v["arbiter_confirmed_at"], Value::Null);
}

#[tokio::test]
async fn the_key_is_fixed_when_the_escrow_is_created() {
    let app = app().await;
    let (seller, buyer) = (app.register().await, app.register().await);
    let judge = app.arbiter(5).await;
    let (_, e) = app.create(&seller, Some(&buyer), Some(&judge)).await;
    let id = e["id"].as_str().unwrap();

    // The arbiter (or anyone who took over the account) registers a different key afterwards.
    app.call(Method::PUT, "/api/v1/me/arbiter-key", Some(&judge.token), Some(json!({"public_key": spki(9)}))).await;
    let (_, v) = app.call(Method::GET, &format!("/api/v1/escrows/{id}"), Some(&buyer.token), None).await;
    assert_eq!(v["arbiter_key"], spki(5).as_str(), "the buyer still encrypts to the key recorded at creation");
}

#[tokio::test]
async fn an_arbitrated_escrow_cannot_be_accepted_without_a_sealed_secret_and_the_blob_stays_private() {
    let app = app().await;
    let (seller, buyer, other) = (app.register().await, app.register().await, app.register().await);
    let judge = app.arbiter(5).await;
    let (_, e) = app.create(&seller, Some(&buyer), Some(&judge)).await;
    let id = e["id"].as_str().unwrap().to_string();
    let p = secret(1);

    let (s, v) = app.accept(&buyer, &id, &p, None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid_request")), "{v}");
    let (s, _) = app.accept(&buyer, &id, &p, Some("tiny".into())).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(app.state(&buyer.token, &id).await, "CREATED");
    assert_eq!(app.node.state(&p.hash()), None, "no invoice was made");

    let blob = sealed(7);
    let (s, v) = app.accept(&buyer, &id, &p, Some(blob.clone())).await;
    assert_eq!((s, v["state"].as_str(), v["arbiter_sealed"].as_bool()), (StatusCode::OK, Some("INVOICE_ISSUED"), Some(true)), "{v}");

    // No view ever carries the blob; only the arbiter can fetch it.
    for who in [&buyer, &seller, &judge] {
        let (_, v) = app.call(Method::GET, &format!("/api/v1/escrows/{id}"), Some(&who.token), None).await;
        assert!(!v.to_string().contains(&blob), "the sealed secret leaked into an escrow view");
        let (_, ev) = app.call(Method::GET, &format!("/api/v1/escrows/{id}/events"), Some(&who.token), None).await;
        assert!(!ev.to_string().contains(&blob));
    }
    let path = format!("/api/v1/escrows/{id}/arbitration");
    let (s, v) = app.call(Method::GET, &path, Some(&judge.token), None).await;
    assert_eq!((s, v["sealed_secret"].as_str(), v["payment_hash"].as_str()), (StatusCode::OK, Some(blob.as_str()), Some(hex::encode(p.hash()).as_str())));
    for who in [&buyer, &seller] {
        assert_eq!(app.call(Method::GET, &path, Some(&who.token), None).await.0, StatusCode::FORBIDDEN);
    }
    assert_eq!(app.call(Method::GET, &path, Some(&other.token), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(app.call(Method::GET, &path, Some(OPERATOR_TOKEN), None).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_escrow_without_an_arbiter_refuses_a_sealed_secret() {
    let app = app().await;
    let (seller, buyer) = (app.register().await, app.register().await);
    let (_, e) = app.create(&seller, Some(&buyer), None).await;
    let (s, _) = app.accept(&buyer, e["id"].as_str().unwrap(), &secret(1), Some(sealed(1))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_arbiter_confirms_and_both_parties_can_see_it() {
    let app = app().await;
    let (seller, buyer) = (app.register().await, app.register().await);
    let judge = app.arbiter(5).await;
    let p = secret(1);
    let id = app.locked(&seller, &buyer, &judge, &p).await;
    let path = format!("/api/v1/escrows/{id}/arbitration/confirm");

    for who in [&buyer, &seller] {
        assert_eq!(app.call(Method::POST, &path, Some(&who.token), None).await.0, StatusCode::FORBIDDEN);
    }
    app.clock.store(T0 + 30, Ordering::SeqCst);
    let (s, v) = app.call(Method::POST, &path, Some(&judge.token), None).await;
    assert_eq!((s, v["arbiter_confirmed_at"].as_i64(), v["your_role"].as_str()), (StatusCode::OK, Some(T0 + 30), Some("arbiter")), "{v}");
    for who in [&buyer, &seller] {
        let (_, v) = app.call(Method::GET, &format!("/api/v1/escrows/{id}"), Some(&who.token), None).await;
        assert_eq!(v["arbiter_confirmed_at"].as_i64(), Some(T0 + 30));
    }
    // It shows up in the arbiter's own list, in their role.
    let (_, list) = app.call(Method::GET, "/api/v1/escrows", Some(&judge.token), None).await;
    assert_eq!(list["escrows"][0]["your_role"], "arbiter");
}

#[tokio::test]
async fn a_disputed_escrow_is_released_to_the_seller_on_the_arbiters_say_so() {
    let app = app().await;
    let (seller, buyer) = (app.register().await, app.register().await);
    let judge = app.arbiter(5).await;
    let p = secret(1);
    let id = app.locked(&seller, &buyer, &judge, &p).await;

    // Before a dispute the arbiter has no say.
    let (s, v) = app.decide(&judge, &id, json!({"decision": "refund"})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("invalid_state")));
    app.dispute(&seller, &id).await;

    // The buyer has not answered the dispute yet, so the arbiter has to wait: a quarter of the hold
    // that was left (3600 s), so 900 s. Nothing is decided in the meantime.
    let (s, v) = app.decide(&judge, &id, json!({"decision": "refund"})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("response_window")), "{v}");
    assert_eq!(app.state(&seller.token, &id).await, "DISPUTED");
    let (_, ev) = app.call(Method::GET, &format!("/api/v1/escrows/{id}/events"), Some(&seller.token), None).await;
    assert_eq!(ev["decide_after"], T0 + 900, "{ev}");
    app.clock.fetch_add(901, Ordering::SeqCst);

    // Bad requests change nothing.
    let wrong = hex::encode(secret(9).as_bytes());
    for (body, status, code) in [
        (json!({"decision": "release", "preimage": wrong}), StatusCode::UNPROCESSABLE_ENTITY, "wrong_preimage"),
        (json!({"decision": "release", "preimage": "123456"}), StatusCode::BAD_REQUEST, "invalid_preimage"),
        (json!({"decision": "release"}), StatusCode::BAD_REQUEST, "invalid_preimage"),
        (json!({"decision": "maybe"}), StatusCode::BAD_REQUEST, "invalid_decision"),
    ] {
        let (s, v) = app.decide(&judge, &id, body).await;
        assert_eq!((s, v["error"]["code"].as_str()), (status, Some(code)), "{v}");
    }
    assert_eq!(app.state(&seller.token, &id).await, "DISPUTED");

    // Nobody but the arbiter can decide, with or without the secret.
    let right = json!({"decision": "release", "preimage": hex::encode(p.as_bytes())});
    for who in [&buyer, &seller] {
        assert_eq!(app.decide(who, &id, right.clone()).await.0, StatusCode::FORBIDDEN);
    }

    let (s, v) = app.decide(&judge, &id, json!({"decision": "release", "preimage": hex::encode(p.as_bytes()), "note": "photo is genuine"})).await;
    assert_eq!((s, v["state"].as_str()), (StatusCode::OK, Some("SETTLED")), "{v}");
    assert_eq!(app.node.state(&p.hash()), Some(NodeInvoiceState::Settled));
    let (_, ev) = app.call(Method::GET, &format!("/api/v1/escrows/{id}/events"), Some(&buyer.token), None).await;
    let decision = ev["audit"].as_array().unwrap().iter().find(|a| a["action"] == "arbiter_decision").unwrap();
    assert_eq!(decision["actor"], judge.id.as_str());
    assert!(decision["reason"].as_str().unwrap().contains("photo is genuine"));
    // The secret is not echoed back anywhere.
    assert!(!ev.to_string().contains(&hex::encode(p.as_bytes())));
}

#[tokio::test]
async fn a_disputed_escrow_is_refunded_to_the_buyer_on_the_arbiters_say_so() {
    let app = app().await;
    let (seller, buyer) = (app.register().await, app.register().await);
    let judge = app.arbiter(5).await;
    let p = secret(1);
    let id = app.locked(&seller, &buyer, &judge, &p).await;
    app.dispute(&buyer, &id).await;

    // Until the seller answers, the arbiter waits. The moment the seller adds their side the wait is over.
    let (s, v) = app.decide(&judge, &id, json!({"decision": "refund"})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("response_window")), "{v}");
    let (s, v) = app
        .call(Method::POST, &format!("/api/v1/escrows/{id}/notes"), Some(&seller.token), Some(json!({"kind": "evidence", "text": "courier receipt NG123"})))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, ev) = app.call(Method::GET, &format!("/api/v1/escrows/{id}/events"), Some(&buyer.token), None).await;
    assert!(ev["decide_after"].is_null(), "{ev}");

    let (s, v) = app.decide(&judge, &id, json!({"decision": "refund", "note": "no proof of delivery"})).await;
    assert_eq!((s, v["state"].as_str()), (StatusCode::OK, Some("REFUNDED")), "{v}");
    assert_eq!(app.node.state(&p.hash()), Some(NodeInvoiceState::Canceled));
}

#[tokio::test]
async fn after_the_deadline_the_arbiter_is_too_late_and_the_buyer_is_refunded() {
    let app = app().await;
    let (seller, buyer) = (app.register().await, app.register().await);
    let judge = app.arbiter(5).await;
    let p = secret(1);
    let id = app.locked(&seller, &buyer, &judge, &p).await;
    app.dispute(&seller, &id).await;

    app.clock.store(T0 + 3600, Ordering::SeqCst);
    let (s, v) = app.decide(&judge, &id, json!({"decision": "release", "preimage": hex::encode(p.as_bytes())})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("too_late")));
    app.service.tick(T0 + 3600).await.unwrap();
    assert_eq!(app.state(&buyer.token, &id).await, "REFUNDED");
}

#[tokio::test]
async fn the_arbiter_has_no_other_powers_and_the_operator_cannot_overrule_them() {
    let app = app().await;
    let (seller, buyer) = (app.register().await, app.register().await);
    let judge = app.arbiter(5).await;
    let p = secret(1);
    let id = app.locked(&seller, &buyer, &judge, &p).await;

    // The arbiter acts only through their decision: not release, cancel, dispute or accept.
    let release = json!({"preimage": hex::encode(p.as_bytes())});
    for (path, body) in [
        (format!("/api/v1/escrows/{id}/release"), Some(release)),
        (format!("/api/v1/escrows/{id}/cancel"), None),
        (format!("/api/v1/escrows/{id}/dispute"), Some(json!({"reason": "x"}))),
        (format!("/api/v1/escrows/{id}/accept"), Some(json!({"payment_hash": hex::encode(p.hash())}))),
    ] {
        let (s, _) = app.call(Method::POST, &path, Some(&judge.token), body).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{path}");
    }
    assert_eq!(app.state(&judge.token, &id).await, "LOCKED");

    // Once disputed, the operator cannot cancel over the arbiter\'s head.
    app.dispute(&buyer, &id).await;
    let (s, _) = app.call(Method::POST, &format!("/api/v1/escrows/{id}/cancel"), Some(OPERATOR_TOKEN), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(app.state(&judge.token, &id).await, "DISPUTED");

    // An escrow with no arbiter is unchanged: the operator still resolves its disputes.
    let (_, e) = app.create(&seller, Some(&buyer), None).await;
    let plain = e["id"].as_str().unwrap().to_string();
    let q = secret(2);
    app.accept(&buyer, &plain, &q, None).await;
    app.pay(&q).await;
    app.dispute(&buyer, &plain).await;
    let (s, v) = app.call(Method::POST, &format!("/api/v1/escrows/{plain}/cancel"), Some(OPERATOR_TOKEN), None).await;
    assert_eq!((s, v["state"].as_str()), (StatusCode::OK, Some("REFUNDED")), "{v}");
}
