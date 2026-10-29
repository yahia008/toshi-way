//! Delivery claims and evidence notes over the HTTP API (mock node, in-memory store).

use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Router,
};
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

    /// An escrow the buyer has accepted and paid.
    async fn locked(&self, seller: &Account, buyer: &Account, p: &Preimage) -> String {
        let (s, e) = self.create(seller, Some(buyer), None).await;
        assert_eq!(s, StatusCode::CREATED, "{e}");
        let id = e["id"].as_str().unwrap().to_string();
        assert_eq!(self.accept(buyer, &id, p, None).await.0, StatusCode::OK);
        self.pay(p).await;
        id
    }

    async fn note(&self, who: &Account, id: &str, kind: &str, text: &str) -> (StatusCode, Value) {
        self.call(Method::POST, &format!("/api/v1/escrows/{id}/notes"), Some(&who.token), Some(json!({"kind": kind, "text": text}))).await
    }

    async fn audit(&self, who: &Account, id: &str) -> Vec<Value> {
        let (_, v) = self.call(Method::GET, &format!("/api/v1/escrows/{id}/events"), Some(&who.token), None).await;
        v["audit"].as_array().unwrap().clone()
    }
}

fn code(v: &Value) -> Option<&str> {
    v["error"]["code"].as_str()
}

#[tokio::test]
async fn the_seller_marks_delivery_and_both_sides_see_it_without_any_state_change() {
    let app = app().await;
    let (seller, buyer) = (app.register().await, app.register().await);
    let p = secret(1);
    let id = app.locked(&seller, &buyer, &p).await;

    let (s, v) = app.note(&seller, &id, "delivered", "  Signed for by J. Okafor, tracking NG123  ").await;
    assert_eq!((s, v["state"].as_str()), (StatusCode::OK, Some("LOCKED")), "{v}");

    for who in [&seller, &buyer] {
        let audit = app.audit(who, &id).await;
        let note = audit.iter().find(|a| a["action"] == "delivered").expect("the claim is on the history");
        assert_eq!(note["actor"], seller.id.as_str());
        assert_eq!(note["reason"], "Signed for by J. Okafor, tracking NG123");
        assert_eq!((note["from_state"].as_str(), note["to_state"].as_str()), (Some("LOCKED"), Some("LOCKED")));
    }
    // The escrow itself says when delivery was claimed, for both sides and for the dashboard.
    for who in [&seller, &buyer] {
        let (_, e) = app.call(Method::GET, &format!("/api/v1/escrows/{id}"), Some(&who.token), None).await;
        assert_eq!(e["delivered_at"], T0, "{e}");
    }
    // Saying it twice is refused, and evidence does not count as a delivery claim.
    let (s, v) = app.note(&seller, &id, "delivered", "done again").await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, Some("invalid_request")), "{v}");
    // It moves no money: the node still holds the payment.
    assert_eq!(app.node.state(&p.hash()), Some(NodeInvoiceState::Accepted));

    // Only the seller can say it was delivered.
    let (s, v) = app.note(&buyer, &id, "delivered", "I got it").await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, Some("forbidden")), "{v}");
}

#[tokio::test]
async fn both_sides_can_add_evidence_before_and_after_a_dispute_and_nobody_else_can() {
    let app = app().await;
    let (seller, buyer, stranger) = (app.register().await, app.register().await, app.register().await);
    let id = app.locked(&seller, &buyer, &secret(2)).await;

    assert_eq!(app.note(&seller, &id, "evidence", "courier receipt: https://example.com/r/1").await.0, StatusCode::OK);
    assert_eq!(app.note(&buyer, &id, "evidence", "the box was empty").await.0, StatusCode::OK);
    let (_, e) = app.call(Method::GET, &format!("/api/v1/escrows/{id}"), Some(&seller.token), None).await;
    assert!(e["delivered_at"].is_null(), "evidence is not a delivery claim: {e}");

    let (s, _) = app.call(Method::POST, &format!("/api/v1/escrows/{id}/dispute"), Some(&buyer.token), Some(json!({"reason": "empty box"}))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = app.note(&seller, &id, "evidence", "weight at dispatch: 4.2 kg").await;
    assert_eq!((s, v["state"].as_str()), (StatusCode::OK, Some("DISPUTED")), "{v}");

    // A stranger does not even learn the escrow exists.
    assert_eq!(app.note(&stranger, &id, "evidence", "hello").await.0, StatusCode::NOT_FOUND);

    let who_said: Vec<_> = app.audit(&buyer, &id).await.into_iter().filter(|a| a["action"] == "evidence").map(|a| a["actor"].as_str().unwrap().to_string()).collect();
    assert_eq!(who_said, [seller.id.clone(), buyer.id.clone(), seller.id.clone()]);
}

#[tokio::test]
async fn bad_notes_are_refused_and_a_note_needs_a_locked_escrow_before_its_deadline() {
    let app = app().await;
    let (seller, buyer) = (app.register().await, app.register().await);

    // Nothing is locked yet, so there is nothing to give evidence about.
    let (s, e) = app.create(&seller, Some(&buyer), None).await;
    assert_eq!(s, StatusCode::CREATED);
    let id = e["id"].as_str().unwrap().to_string();
    let (s, v) = app.note(&seller, &id, "delivered", "done").await;
    assert_eq!((s, code(&v)), (StatusCode::CONFLICT, Some("invalid_state")), "{v}");

    let p = secret(3);
    assert_eq!(app.accept(&buyer, &id, &p, None).await.0, StatusCode::OK);
    app.pay(&p).await;

    for (kind, text, status, want) in [
        ("shipped", "x", StatusCode::BAD_REQUEST, "invalid_kind"),
        ("evidence", "   ", StatusCode::BAD_REQUEST, "invalid_note"),
        ("evidence", &"x".repeat(501), StatusCode::BAD_REQUEST, "invalid_note"),
    ] {
        let (s, v) = app.note(&seller, &id, kind, text).await;
        assert_eq!((s, code(&v)), (status, Some(want)), "{v}");
    }

    // After the settle-by time the hold is over, as it is for a dispute.
    app.clock.fetch_add(4000, Ordering::SeqCst);
    let (s, v) = app.note(&seller, &id, "evidence", "too late").await;
    assert_eq!((s, code(&v)), (StatusCode::CONFLICT, Some("too_late")), "{v}");
}

#[tokio::test]
async fn notes_are_capped_per_person_so_a_history_cannot_be_flooded() {
    let app = app().await;
    let (seller, buyer) = (app.register().await, app.register().await);
    let id = app.locked(&seller, &buyer, &secret(4)).await;
    for n in 0..10 {
        assert_eq!(app.note(&seller, &id, "evidence", &format!("item {n}")).await.0, StatusCode::OK);
    }
    let (s, v) = app.note(&seller, &id, "evidence", "one more").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    // The cap is per person: the buyer still has their say.
    assert_eq!(app.note(&buyer, &id, "evidence", "my side").await.0, StatusCode::OK);
}

#[tokio::test]
async fn a_read_only_api_key_cannot_add_notes() {
    let app = app().await;
    let (seller, buyer) = (app.register().await, app.register().await);
    let id = app.locked(&seller, &buyer, &secret(5)).await;
    let (s, k) = app.call(Method::POST, "/api/v1/me/api-keys", Some(&seller.token), Some(json!({"name": "reader", "scopes": ["escrows:read"]}))).await;
    assert_eq!(s, StatusCode::CREATED, "{k}");
    let key = Account { id: seller.id.clone(), token: k["key"].as_str().unwrap().into() };
    assert_eq!(app.note(&key, &id, "delivered", "done").await.0, StatusCode::FORBIDDEN);

    let (s, k) = app.call(Method::POST, "/api/v1/me/api-keys", Some(&seller.token), Some(json!({"name": "writer", "scopes": ["escrows:write"]}))).await;
    assert_eq!(s, StatusCode::CREATED, "{k}");
    let key = Account { id: seller.id.clone(), token: k["key"].as_str().unwrap().into() };
    assert_eq!(app.note(&key, &id, "delivered", "done").await.0, StatusCode::OK);
}
