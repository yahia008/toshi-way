//! Balances, withdrawals and the naira ramp over HTTP, against the mock node and wallets.

use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Router,
};
use escrow_api::{ensure_operator, rates::FixedRates, router, ApiConfig, AppState};
use escrow_core::{
    mock::{MockLightning, MockWallet},
    model::Preimage,
    payout::{PaymentStatus, PayoutService},
    ramp::{DemoRamp, RampConfig, RampService},
    service::EscrowService,
    store::{MemoryStore, PayoutStore},
    timing::TimingConfig,
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const T0: i64 = 1_000_000;
/// A withdrawal PIN for the tests' sellers.
const PIN: &str = "482915";
const OPERATOR_TOKEN: &str = "operator-token-for-tests-0123456789";

struct App {
    router: Router,
    store: Arc<MemoryStore>,
    node: MockLightning,
    service: Arc<EscrowService<MockLightning, Arc<MemoryStore>>>,
    payouts: Option<Arc<PayoutService<Arc<MemoryStore>>>>,
    ramp: Option<Arc<RampService<Arc<MemoryStore>>>>,
    shared: MockWallet,
    ramp_wallet: MockWallet,
    clock: Arc<AtomicI64>,
}

struct Account {
    id: String,
    token: String,
}

/// `money` turns on balances and the demo naira ramp; without it the server offers neither.
async fn app(money: bool) -> App {
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
    let rates = Arc::new(FixedRates::parse("NGN=97500000").unwrap());
    let mut state = AppState::new(service.clone(), store.clone(), config, now).with_rates(rates);
    let (shared, ramp_wallet) = (MockWallet::new(), MockWallet::new());
    let (mut payouts, mut ramp) = (None, None);
    if money {
        let p = Arc::new(PayoutService::new(Arc::new(shared.clone()), store.clone()));
        let r = Arc::new(RampService::new(
            Arc::new(DemoRamp::new(Arc::new(ramp_wallet.clone()))),
            Some(p.clone()),
            store.clone(),
            RampConfig::default(),
        ));
        state = state.with_payouts(p.clone()).with_ramp(r.clone());
        (payouts, ramp) = (Some(p), Some(r));
    }
    App { router: router(state), store, node, service, payouts, ramp, shared, ramp_wallet, clock }
}

impl App {
    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }

    async fn call(&self, method: Method, path: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => req.header("content-type", "application/json").body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let res = self.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn set_pin(&self, who: &Account) {
        let (s, v) = self.call(Method::PUT, "/api/v1/me/withdrawal-pin", Some(&who.token), Some(json!({"pin": PIN}))).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }

    async fn register(&self) -> Account {
        let (s, v) = self.call(Method::POST, "/api/v1/users", None, None).await;
        assert_eq!(s, StatusCode::CREATED);
        Account { id: v["id"].as_str().unwrap().into(), token: v["token"].as_str().unwrap().into() }
    }

    /// The seller offers `amount_sats`, the buyer accepts: the invoice is issued. Returns its id.
    async fn issued(&self, seller: &Account, buyer: &Account, amount_sats: u64, p: &Preimage) -> String {
        let (s, v) = self
            .call(
                Method::POST,
                "/api/v1/escrows",
                Some(&seller.token),
                Some(json!({"buyer_id": buyer.id, "amount_sats": amount_sats, "description": "parcel", "hold_secs": 3600})),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        let id = v["id"].as_str().unwrap().to_string();
        let (s, v) = self
            .call(
                Method::POST,
                &format!("/api/v1/escrows/{id}/accept"),
                Some(&buyer.token),
                Some(json!({"payment_hash": hex::encode(p.hash())})),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        id
    }

    /// A released escrow on the shared node: the seller is owed `amount_sats`.
    async fn earn(&self, seller: &Account, buyer: &Account, amount_sats: u64, n: u8) {
        let p = Preimage::from_bytes([n; 32]);
        let id = self.issued(seller, buyer, amount_sats, &p).await;
        self.service.apply_event(self.node.pay(&p.hash()), self.now()).await.unwrap();
        let (s, v) = self
            .call(
                Method::POST,
                &format!("/api/v1/escrows/{id}/release"),
                Some(&seller.token),
                Some(json!({"preimage": hex::encode(p.as_bytes())})),
            )
            .await;
        assert_eq!((s, v["state"].as_str()), (StatusCode::OK, Some("SETTLED")), "{v}");
    }

    /// One pass of the background loop.
    async fn run_loops(&self) {
        self.payouts.as_ref().unwrap().sync(self.now()).await.unwrap();
        self.ramp.as_ref().unwrap().tick(self.now()).await.unwrap();
    }
}

#[tokio::test]
async fn without_a_payout_node_there_are_no_balances_or_ramp() {
    let app = app(false).await;
    let seller = app.register().await;
    let (s, v) = app.call(Method::GET, "/api/v1/me/balance", Some(&seller.token), None).await;
    assert_eq!((s, v), (StatusCode::OK, json!({"payouts": false})));
    let (s, v) = app.call(Method::GET, "/api/v1/ramp", Some(&seller.token), None).await;
    assert_eq!((s, v), (StatusCode::OK, json!({"enabled": false})));
    let (s, v) = app.call(Method::POST, "/api/v1/me/withdrawals", Some(&seller.token), Some(json!({"invoice": "lnbcrt1", "pin": PIN}))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("payouts_unavailable")));
}

#[tokio::test]
async fn a_seller_sees_their_balance_and_withdraws_it_over_lightning() {
    let app = app(true).await;
    let (seller, buyer) = (app.register().await, app.register().await);
    app.earn(&seller, &buyer, 20_000, 1).await;
    app.set_pin(&seller).await;

    let (s, b) = app.call(Method::GET, "/api/v1/me/balance", Some(&seller.token), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!((b["available_sats"].as_u64(), b["earned_sats"].as_u64(), b["offramp"].as_bool()), (Some(20_000), Some(20_000), Some(true)));
    let (_, b) = app.call(Method::GET, "/api/v1/me/balance", Some(&buyer.token), None).await;
    assert_eq!(b["available_sats"], 0, "paying earns nothing");

    let invoice = app.shared.invoice(12_000, T0 + 3600);
    let (s, w) = app.call(Method::POST, "/api/v1/me/withdrawals", Some(&seller.token), Some(json!({"invoice": invoice, "pin": PIN}))).await;
    assert_eq!((s, w["state"].as_str(), w["amount_sats"].as_u64()), (StatusCode::CREATED, Some("pending"), Some(12_000)), "{w}");
    let id = w["id"].as_str().unwrap().to_string();
    let (_, b) = app.call(Method::GET, "/api/v1/me/balance", Some(&seller.token), None).await;
    assert_eq!((b["available_sats"].as_u64(), b["held_sats"].as_u64()), (Some(7_880), Some(12_120)));

    // Paying the same invoice again, or more than is left, is refused.
    let (s, v) = app.call(Method::POST, "/api/v1/me/withdrawals", Some(&seller.token), Some(json!({"invoice": invoice, "pin": PIN}))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("duplicate_invoice")));
    let big = app.shared.invoice(8_000, T0 + 3600);
    let (s, v) = app.call(Method::POST, "/api/v1/me/withdrawals", Some(&seller.token), Some(json!({"invoice": big, "pin": PIN}))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("insufficient_balance")));
    let (s, v) = app.call(Method::POST, "/api/v1/me/withdrawals", Some(&seller.token), Some(json!({"invoice": "not an invoice", "pin": PIN}))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid_invoice")));

    // The node reports it paid: the withdrawal succeeds and only the real fee is kept.
    let hash = app.shared.hash_of(&invoice).unwrap();
    app.shared.set_payment(&hash, PaymentStatus::Succeeded { fee_sats: 3 });
    app.run_loops().await;
    let (s, w) = app.call(Method::GET, &format!("/api/v1/me/withdrawals/{id}"), Some(&seller.token), None).await;
    assert_eq!((s, w["state"].as_str(), w["fee_sats"].as_u64()), (StatusCode::OK, Some("succeeded"), Some(3)));
    let (_, b) = app.call(Method::GET, "/api/v1/me/balance", Some(&seller.token), None).await;
    assert_eq!(b["available_sats"], 7_997);
    let (_, list) = app.call(Method::GET, "/api/v1/me/withdrawals", Some(&seller.token), None).await;
    assert_eq!(list["withdrawals"].as_array().unwrap().len(), 1);

    // Nobody else can see it, and the operator has no balance to spend.
    let (s, _) = app.call(Method::GET, &format!("/api/v1/me/withdrawals/{id}"), Some(&buyer.token), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = app.call(Method::POST, "/api/v1/me/withdrawals", Some(OPERATOR_TOKEN), Some(json!({"invoice": big}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_ramp_lists_banks_checks_names_and_quotes_in_naira() {
    let app = app(true).await;
    let user = app.register().await;
    let (s, info) = app.call(Method::GET, "/api/v1/ramp", Some(&user.token), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!((info["enabled"].as_bool(), info["demo"].as_bool(), info["currency"].as_str()), (Some(true), Some(true), Some("NGN")));
    assert_eq!(info["rate_minor_per_btc"], 9_750_000_000i64);
    assert!(info["banks"].as_array().unwrap().iter().any(|b| b["name"] == "Guaranty Trust Bank" && b["code"] == "058"));

    let (s, a) = app
        .call(Method::POST, "/api/v1/ramp/resolve", Some(&user.token), Some(json!({"bank_code": "058", "account_number": "0123456789"})))
        .await;
    assert_eq!((s, a["account_name"].as_str()), (StatusCode::OK, Some("DEMO ACCOUNT 6789")));
    let (s, _) = app
        .call(Method::POST, "/api/v1/ramp/resolve", Some(&user.token), Some(json!({"bank_code": "058", "account_number": "12"})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, q) = app
        .call(Method::POST, "/api/v1/ramp/quote", Some(&user.token), Some(json!({"direction": "onramp", "amount_sats": 5_000})))
        .await;
    assert_eq!((s, q["fiat_minor"].as_i64(), q["fee_minor"].as_i64()), (StatusCode::OK, Some(494_813), Some(7_313)));
}

#[tokio::test]
async fn a_buyer_pays_an_escrow_by_bank_transfer() {
    let app = app(true).await;
    let (seller, buyer, stranger) = (app.register().await, app.register().await, app.register().await);
    let p = Preimage::from_bytes([5; 32]);
    let id = app.issued(&seller, &buyer, 5_000, &p).await;
    let refund_to = json!({"bank_code": "058", "account_number": "0123456789"});

    // Only the buyer may pay in naira.
    let (s, _) = app.call(Method::POST, &format!("/api/v1/escrows/{id}/onramp"), Some(&stranger.token), Some(refund_to.clone())).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, o) = app.call(Method::POST, &format!("/api/v1/escrows/{id}/onramp"), Some(&buyer.token), Some(refund_to)).await;
    assert_eq!(s, StatusCode::CREATED, "{o}");
    assert_eq!((o["state"].as_str(), o["fiat_minor"].as_i64()), (Some("awaiting_deposit"), Some(494_813)));
    assert_eq!(o["bank"]["account_number"], "••••••6789", "the refund account is shown masked");
    assert!(o["deposit"]["reference"].as_str().unwrap().starts_with("TW-"));
    let order = o["id"].as_str().unwrap().to_string();

    // The demo lets the buyer say the transfer arrived; the provider then pays the invoice.
    let (s, o) = app.call(Method::POST, &format!("/api/v1/ramp/orders/{order}/demo-deposit"), Some(&buyer.token), None).await;
    assert_eq!((s, o["state"].as_str()), (StatusCode::OK, Some("deposit_received")));
    app.run_loops().await;
    assert_eq!(app.ramp_wallet.paid(), [p.hash()]);
    app.service.apply_event(app.node.pay(&p.hash()), app.now()).await.unwrap();
    app.run_loops().await;
    let (_, v) = app.call(Method::GET, &format!("/api/v1/escrows/{id}/onramp"), Some(&buyer.token), None).await;
    assert_eq!(v["order"]["state"], "funded");
    let (_, e) = app.call(Method::GET, &format!("/api/v1/escrows/{id}"), Some(&seller.token), None).await;
    assert_eq!(e["state"], "LOCKED", "the escrow is locked like any other payment");

    // Someone else's order is not theirs to touch.
    let (s, _) = app.call(Method::POST, &format!("/api/v1/ramp/orders/{order}/demo-deposit"), Some(&seller.token), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, mine) = app.call(Method::GET, "/api/v1/ramp/orders", Some(&buyer.token), None).await;
    assert_eq!(mine["orders"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn the_operator_can_fund_an_escrow_in_naira_for_a_demo() {
    let app = app(true).await;
    let (seller, buyer) = (app.register().await, app.register().await);
    let p = Preimage::from_bytes([6; 32]);
    let id = app.issued(&seller, &buyer, 5_000, &p).await;
    let path = format!("/api/v1/operator/escrows/{id}/demo-onramp");
    let refund_to = json!({"bank_code": "058", "account_number": "0123456789"});

    let (s, _) = app.call(Method::POST, &path, Some(&buyer.token), Some(refund_to.clone())).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "only the operator");
    let (s, o) = app.call(Method::POST, &path, Some(OPERATOR_TOKEN), Some(refund_to)).await;
    assert_eq!((s, o["state"].as_str()), (StatusCode::CREATED, Some("deposit_received")), "{o}");

    // The order is the buyer's, and the provider pays the invoice as for any naira payment.
    let (_, mine) = app.call(Method::GET, "/api/v1/ramp/orders", Some(&buyer.token), None).await;
    assert_eq!(mine["orders"][0]["id"], o["id"]);
    app.run_loops().await;
    assert_eq!(app.ramp_wallet.paid(), [p.hash()]);
}

#[tokio::test]
async fn a_seller_withdraws_their_balance_to_a_nigerian_bank() {
    let app = app(true).await;
    let (seller, buyer) = (app.register().await, app.register().await);
    app.earn(&seller, &buyer, 10_000, 1).await;
    app.set_pin(&seller).await;

    let (s, o) = app
        .call(
            Method::POST,
            "/api/v1/ramp/offramp",
            Some(&seller.token),
            Some(json!({"amount_sats": 6_000, "bank_code": "057", "account_number": "2012345678", "pin": PIN})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{o}");
    assert_eq!((o["state"].as_str(), o["bank"]["bank_name"].as_str()), (Some("sending_sats"), Some("Zenith Bank")));
    let order = o["id"].as_str().unwrap().to_string();
    let (_, b) = app.call(Method::GET, "/api/v1/me/balance", Some(&seller.token), None).await;
    assert_eq!(b["available_sats"], 10_000 - 6_060);

    // The sats reach the provider, which then pays the naira out.
    let (_, list) = app.call(Method::GET, "/api/v1/me/withdrawals", Some(&seller.token), None).await;
    let w = &list["withdrawals"][0];
    assert_eq!(w["kind"], "bank");
    let hash: [u8; 32] = hex::decode(w["payment_hash"].as_str().unwrap()).unwrap().try_into().unwrap();
    app.shared.set_payment(&hash, PaymentStatus::Succeeded { fee_sats: 1 });
    app.run_loops().await;
    app.run_loops().await;
    let (_, o) = app.call(Method::GET, &format!("/api/v1/ramp/orders/{order}"), Some(&seller.token), None).await;
    assert_eq!(o["state"], "paid_out");
    assert!(o["fiat_reference"].as_str().unwrap().starts_with("DEMO-PAYOUT-"));

    // More than the balance is refused.
    let (s, v) = app
        .call(
            Method::POST,
            "/api/v1/ramp/offramp",
            Some(&seller.token),
            Some(json!({"amount_sats": 9_000, "bank_code": "057", "account_number": "2012345678", "pin": PIN})),
        )
        .await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("insufficient_balance")));
}

#[tokio::test]
async fn withdrawals_need_the_pin_and_wrong_guesses_lock_it() {
    let app = app(true).await;
    let (seller, buyer) = (app.register().await, app.register().await);
    app.earn(&seller, &buyer, 20_000, 1).await;
    let (a, token) = (&app, seller.token.clone());
    let withdraw = |pin: &'static str| {
        let (invoice, token) = (a.shared.invoice(1_000, T0 + 3600), token.clone());
        async move { a.call(Method::POST, "/api/v1/me/withdrawals", Some(&token), Some(json!({"invoice": invoice, "pin": pin}))).await }
    };
    let code = |v: &Value| v["error"]["code"].as_str().unwrap_or_default().to_string();

    // A stolen token alone is not enough: with no PIN set, nothing can be withdrawn.
    let (s, v) = withdraw("482915").await;
    assert_eq!((s, code(&v)), (StatusCode::CONFLICT, "pin_not_set".into()));
    let (_, v) = app.call(Method::GET, "/api/v1/me/withdrawal-pin", Some(&seller.token), None).await;
    assert_eq!(v["set"], false);

    // Easy PINs are refused; a good one is kept only as a hash.
    for easy in ["123456", "000000", "12345"] {
        let (s, v) = app.call(Method::PUT, "/api/v1/me/withdrawal-pin", Some(&seller.token), Some(json!({"pin": easy}))).await;
        assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "invalid_pin".into()), "{easy}");
    }
    app.set_pin(&seller).await;
    let stored = app.store.get_withdrawal_pin(&seller.id).await.unwrap().unwrap();
    assert!(stored.pin_hash.starts_with("$argon2") && !stored.pin_hash.contains(PIN));

    // Missing or wrong: refused, and wrong guesses are counted down.
    let (s, v) = app.call(Method::POST, "/api/v1/me/withdrawals", Some(&seller.token), Some(json!({"invoice": app.shared.invoice(1_000, T0 + 3600)}))).await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "pin_required".into()));
    let (s, v) = withdraw("111112").await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "wrong_pin".into()));
    assert!(v["error"]["message"].as_str().unwrap().contains("4 tries left"), "{v}");
    // A right one works and forgets the wrong guesses.
    let (s, v) = withdraw("482915").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");

    // Five wrong in a row lock it, even against the right PIN, until the lock runs out.
    for _ in 0..5 {
        withdraw("111112").await;
    }
    let (s, v) = withdraw("482915").await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "pin_locked".into()));
    let (_, v) = app.call(Method::GET, "/api/v1/me/withdrawal-pin", Some(&seller.token), None).await;
    assert!(v["locked_until"].is_i64());
    app.clock.fetch_add(escrow_api::pin::LOCK_SECS, Ordering::SeqCst);
    let (s, v) = withdraw("482915").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");

    // Changing it needs the current one.
    let (s, v) = app.call(Method::PUT, "/api/v1/me/withdrawal-pin", Some(&seller.token), Some(json!({"pin": "905173"}))).await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "pin_required".into()));
    let (s, _) = app
        .call(Method::PUT, "/api/v1/me/withdrawal-pin", Some(&seller.token), Some(json!({"pin": "905173", "current_pin": PIN})))
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = withdraw("482915").await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "wrong_pin".into()));
}
