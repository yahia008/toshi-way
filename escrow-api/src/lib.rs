//! HTTP API for toshi-way escrows.
//!
//! Handlers never decide payment state: they authenticate, authorize against the escrow's
//! parties, validate input, and call the escrow service, whose state comes from the node.

pub mod arbiter;
pub mod auth;
pub mod checkout;
pub mod error;
pub mod money;
pub mod pin;
pub mod ratelimit;
pub mod rates;
pub mod routes;
pub mod webhooks;

use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderName, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{delete, get, post},
    Router,
};
use escrow_core::{
    adapter::NodeDirectory,
    model::{Role, User},
    payout::PayoutService,
    ramp::RampService,
    service::EscrowService,
    store::{PayoutStore, Store, UserStore},
};
use tokio::sync::Mutex;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    cors::CorsLayer,
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

use crate::{auth::hash_token, error::ApiError, ratelimit::RateLimiter};

#[derive(Debug, Clone)]
pub struct ApiConfig {
    pub max_amount_sats: u64,
    pub max_description_len: usize,
    /// Requests per minute per authenticated caller (or per address when anonymous).
    pub rate_limit_per_min: u32,
    /// Account registrations per minute per address.
    pub register_limit_per_min: u32,
    /// Browser origins allowed to call the API (for example `http://localhost:3001`).
    pub cors_origins: Vec<String>,
    /// The public address of the web app (for example `https://pay.example.com`). With it, the API can
    /// give a business the checkout link for each escrow it makes. Without it there is no such link.
    pub web_base_url: Option<String>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            max_amount_sats: 1_000_000,
            max_description_len: 500,
            rate_limit_per_min: 120,
            register_limit_per_min: 10,
            cors_origins: Vec::new(),
            web_base_url: None,
        }
    }
}

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Cached responses for `Idempotency-Key`, per user and key.
#[derive(Default)]
pub(crate) struct IdempotencyCache {
    pub(crate) entries: Mutex<HashMap<(String, String), (String, serde_json::Value)>>,
}

/// What it takes to offer webhooks: the key that seals signing secrets, and a way to reach receivers.
pub struct WebhookSupport {
    pub key: escrow_lnd::creds::CredentialKey,
    pub sender: Arc<dyn webhooks::Sender>,
}

pub struct AppState<A, S> {
    pub service: Arc<EscrowService<A, S>>,
    pub users: S,
    pub config: Arc<ApiConfig>,
    pub now: Clock,
    pub(crate) limiter: Arc<RateLimiter>,
    pub(crate) idempotency: Arc<IdempotencyCache>,
    /// Changing a seller's node must not interleave with creating an invoice on it: invoice
    /// creation holds this for reading, a node change for writing.
    pub(crate) node_changes: Arc<tokio::sync::RwLock<()>>,
    /// Where display-only fiat values come from. Off unless configured.
    pub(crate) rates: Arc<dyn rates::RateProvider>,
    /// `None` when there is no credential key, in which case webhooks cannot be set up.
    pub(crate) webhooks: Option<Arc<WebhookSupport>>,
    /// Sellers' balances on the shared node. `None` when there is no shared node to pay from.
    pub(crate) payouts: Option<Arc<PayoutService<S>>>,
    /// Paying and getting paid in naira. Off unless a ramp provider is configured.
    pub(crate) ramp: Option<Arc<RampService<S>>>,
}

impl<A, S: Clone> Clone for AppState<A, S> {
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            users: self.users.clone(),
            config: self.config.clone(),
            now: self.now.clone(),
            limiter: self.limiter.clone(),
            idempotency: self.idempotency.clone(),
            node_changes: self.node_changes.clone(),
            rates: self.rates.clone(),
            webhooks: self.webhooks.clone(),
            payouts: self.payouts.clone(),
            ramp: self.ramp.clone(),
        }
    }
}

impl<A, S: Clone> AppState<A, S> {
    pub fn new(service: Arc<EscrowService<A, S>>, users: S, config: ApiConfig, now: Clock) -> Self {
        Self {
            service,
            users,
            config: Arc::new(config),
            now,
            limiter: Arc::new(RateLimiter::default()),
            idempotency: Arc::new(IdempotencyCache::default()),
            node_changes: Arc::new(tokio::sync::RwLock::new(())),
            rates: Arc::new(rates::Disabled),
            webhooks: None,
            payouts: None,
            ramp: None,
        }
    }

    /// Hold balances for shared-node sellers and let them withdraw.
    pub fn with_payouts(mut self, payouts: Arc<PayoutService<S>>) -> Self {
        self.payouts = Some(payouts);
        self
    }

    /// Offer paying and getting paid in local currency through `ramp`.
    pub fn with_ramp(mut self, ramp: Arc<RampService<S>>) -> Self {
        self.ramp = Some(ramp);
        self
    }

    /// Offer webhooks, sealing signing secrets under `support.key`.
    pub fn with_webhooks(mut self, support: WebhookSupport) -> Self {
        self.webhooks = Some(Arc::new(support));
        self
    }

    /// Use `rates` for display-only fiat values (the default is none).
    pub fn with_rates(mut self, rates: Arc<dyn rates::RateProvider>) -> Self {
        self.rates = rates;
        self
    }
}

/// Make sure an operator account exists for `token`. Used at startup.
pub async fn ensure_operator<U: UserStore>(users: &U, token: &str, now: i64) -> Result<(), ApiError> {
    let hash = hash_token(token);
    if users.user_by_token_hash(&hash).await?.is_some() {
        return Ok(());
    }
    let id = format!("operator_{}", &hash[..8]);
    users.insert_user(User { id, role: Role::Operator, created_at: now }, hash).await?;
    Ok(())
}

async fn rate_limit<A, S>(State(st): State<AppState<A, S>>, req: Request, next: Next) -> Response
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + Clone + 'static,
{
    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .unwrap_or_else(|| "unknown".into());
    let is_register = req.uri().path() == "/api/v1/users";
    let (key, limit) = if is_register {
        (format!("register:{ip}"), st.config.register_limit_per_min)
    } else {
        let caller = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(|v| format!("token:{}", hash_token(v)))
            .unwrap_or_else(|| format!("ip:{ip}"));
        (caller, st.config.rate_limit_per_min)
    };
    match st.limiter.check(&key, limit, (st.now)()) {
        Ok(()) => next.run(req).await,
        Err(retry) => {
            let mut e = ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "too many requests");
            e.retry_after = Some(retry);
            axum::response::IntoResponse::into_response(e)
        }
    }
}

pub fn router<A, S>(state: AppState<A, S>) -> Router
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    let request_id = HeaderName::from_static("x-request-id");
    let cors = {
        let origins: Vec<HeaderValue> =
            state.config.cors_origins.iter().filter_map(|o| HeaderValue::from_str(o).ok()).collect();
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, HeaderName::from_static("idempotency-key")])
            .expose_headers([request_id.clone(), header::RETRY_AFTER])
            .max_age(Duration::from_secs(600))
    };
    Router::new()
        .route("/health", get(routes::health::<A, S>))
        .route("/api/v1/users", post(routes::register::<A, S>))
        .route("/api/v1/me", get(routes::me))
        .route("/api/v1/rates", get(routes::rates_list::<A, S>))
        .route("/api/v1/rates/{currency}", get(routes::rates_one::<A, S>))
        .route(
            "/api/v1/me/arbiter-key",
            get(routes::arbiter_key_get::<A, S>).put(routes::arbiter_key_put::<A, S>),
        )
        .route(
            "/api/v1/me/node",
            get(routes::node_get::<A, S>).put(routes::node_put::<A, S>).delete(routes::node_delete::<A, S>),
        )
        .route("/api/v1/me/api-keys", get(routes::api_key_list::<A, S>).post(routes::api_key_create::<A, S>))
        .route("/api/v1/me/api-keys/{id}", delete(routes::api_key_revoke::<A, S>))
        .route("/api/v1/me/webhooks", get(routes::webhook_list::<A, S>).post(routes::webhook_create::<A, S>))
        .route("/api/v1/me/webhooks/{id}", delete(routes::webhook_delete::<A, S>))
        .route("/api/v1/me/webhooks/{id}/test", post(routes::webhook_test::<A, S>))
        .route("/api/v1/escrows", post(routes::create_escrow::<A, S>).get(routes::list_mine::<A, S>))
        .route("/api/v1/escrows/{id}", get(routes::get_escrow::<A, S>))
        .route("/api/v1/escrows/{id}/accept", post(routes::accept::<A, S>))
        .route("/api/v1/escrows/{id}/invoice", get(routes::invoice::<A, S>))
        .route("/api/v1/escrows/{id}/release", post(routes::release::<A, S>))
        .route("/api/v1/escrows/{id}/cancel", post(routes::cancel::<A, S>))
        .route("/api/v1/escrows/{id}/dispute", post(routes::dispute::<A, S>))
        .route("/api/v1/escrows/{id}/notes", post(routes::add_note::<A, S>))
        .route("/api/v1/escrows/{id}/events", get(routes::events::<A, S>))
        .route("/api/v1/escrows/{id}/arbitration", get(routes::arbitration_get::<A, S>))
        .route("/api/v1/escrows/{id}/arbitration/confirm", post(routes::arbitration_confirm::<A, S>))
        .route("/api/v1/escrows/{id}/arbitration/decide", post(routes::arbitration_decide::<A, S>))
        .route("/api/v1/operator/escrows", get(routes::operator_list::<A, S>))
        .route("/api/v1/operator/escrows/{id}/demo-onramp", post(money::operator_demo_onramp::<A, S>))
        .route("/api/v1/me/balance", get(money::balance::<A, S>))
        .route("/api/v1/me/withdrawals", get(money::withdrawal_list::<A, S>).post(money::withdrawal_create::<A, S>))
        .route("/api/v1/me/withdrawals/{id}", get(money::withdrawal_get::<A, S>))
        .route("/api/v1/me/withdrawal-pin", get(money::pin_get::<A, S>).put(money::pin_set::<A, S>))
        .route("/api/v1/ramp", get(money::ramp_info::<A, S>))
        .route("/api/v1/ramp/resolve", post(money::ramp_resolve::<A, S>))
        .route("/api/v1/ramp/quote", post(money::ramp_quote::<A, S>))
        .route("/api/v1/ramp/offramp", post(money::offramp_start::<A, S>))
        .route("/api/v1/ramp/orders", get(money::ramp_orders::<A, S>))
        .route("/api/v1/ramp/orders/{id}", get(money::ramp_order_get::<A, S>))
        .route("/api/v1/ramp/orders/{id}/demo-deposit", post(money::ramp_demo_deposit::<A, S>))
        .route("/api/v1/escrows/{id}/onramp", get(money::onramp_get::<A, S>).post(money::onramp_start::<A, S>))
        .layer(middleware::from_fn_with_state(state.clone(), rate_limit::<A, S>))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024))
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, Duration::from_secs(30)))
        // Request bodies are never logged: they can carry the release preimage.
        .layer(TraceLayer::new_for_http())
        .layer(PropagateRequestIdLayer::new(request_id.clone()))
        .layer(SetRequestIdLayer::new(request_id, MakeRequestUuid))
        // Outermost, so browser preflight requests are answered before auth and rate limiting.
        .layer(cors)
        .with_state(state)
}
