//! Sellers' balances and withdrawals, and paying or getting paid in naira through the ramp provider.
//!
//! Only escrows on the service's shared node make a balance: the operator received those sats and owes
//! them to the seller. A seller with their own node was paid straight to it and has nothing held here.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use escrow_core::{
    adapter::NodeDirectory,
    fiat,
    model::{RampDirection, RampOrder, Role, User, Withdrawal, WithdrawalKind},
    payout::{PayoutError, PayoutService},
    ramp::{RampError, RampService},
    store::{PayoutStore, Store, UserStore},
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{auth::Auth, error::ApiError, rates::RateError, routes::ApiJson, AppState};

fn payouts_unavailable() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "payouts_unavailable", "this server holds no balances (it has no shared node to pay from)")
}

fn ramp_unavailable() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "ramp_unavailable", "paying in naira is not offered on this server")
}

fn no_operator(user: &User) -> Result<(), ApiError> {
    if user.role == Role::Operator {
        return Err(ApiError::forbidden("the operator account has no balance or bank payouts"));
    }
    Ok(())
}

fn payouts<A, S>(st: &AppState<A, S>) -> Result<&PayoutService<S>, ApiError> {
    st.payouts.as_deref().ok_or_else(payouts_unavailable)
}

fn ramp<A, S>(st: &AppState<A, S>) -> Result<&RampService<S>, ApiError> {
    st.ramp.as_deref().ok_or_else(ramp_unavailable)
}

fn withdrawal_view(w: &Withdrawal) -> Value {
    json!({
        "id": w.id,
        "kind": w.kind.as_str(),
        "amount_sats": w.amount_sats,
        "fee_limit_sats": w.fee_limit_sats,
        "fee_sats": w.fee_sats,
        "state": w.state.as_str(),
        "failure": w.failure,
        "payment_hash": hex::encode(w.payment_hash),
        "created_at": w.created_at,
        "updated_at": w.updated_at,
    })
}

/// Only the last four digits of an account number, as banks print them.
fn masked(account_number: &str) -> String {
    let n = account_number.len();
    format!("{}{}", "•".repeat(n.saturating_sub(4)), &account_number[n.saturating_sub(4)..])
}

pub(crate) fn ramp_view(o: &RampOrder) -> Value {
    json!({
        "id": o.id,
        "direction": o.direction.as_str(),
        "escrow_id": o.escrow_id,
        "withdrawal_id": o.withdrawal_id,
        "amount_sats": o.amount_sats,
        "currency": o.currency,
        "exponent": fiat::exponent(&o.currency),
        "fiat_minor": o.fiat_minor,
        "fee_minor": o.fee_minor,
        "rate_minor_per_btc": o.rate_minor_per_btc,
        "bank": {
            "bank_code": o.bank.bank_code,
            "bank_name": o.bank.bank_name,
            "account_number": masked(&o.bank.account_number),
            "account_name": o.bank.account_name,
        },
        "deposit": o.deposit.as_ref().map(|d| json!({
            "bank_name": d.bank_name,
            "account_number": d.account_number,
            "account_name": d.account_name,
            "reference": d.reference,
            "expires_at": d.expires_at,
        })),
        "provider": o.provider,
        "fiat_reference": o.fiat_reference,
        "state": o.state.as_str(),
        "note": o.note,
        "created_at": o.created_at,
        "updated_at": o.updated_at,
    })
}

// --- Balance and withdrawals ------------------------------------------------------------------

/// What the operator owes the caller, and whether they can withdraw it here.
pub async fn balance<A, S>(State(st): State<AppState<A, S>>, Auth(user): Auth) -> Result<Json<Value>, ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    let Some(p) = st.payouts.as_deref() else {
        return Ok(Json(json!({ "payouts": false })));
    };
    let t = p.balance(&user.id, (st.now)()).await?;
    let limits = p.limits();
    Ok(Json(json!({
        "payouts": true,
        "available_sats": t.available(),
        "earned_sats": t.earned_sats,
        // Paid in naira and settled less than `naira_hold_hours` ago: a bank transfer can still be reversed.
        "maturing_sats": t.maturing_sats,
        "limits": {
            "naira_hold_hours": limits.naira_hold_secs / 3600,
            "window_hours": limits.window_secs / 3600,
            "window_cap_sats": limits.window_cap_sats,
            "left_in_window_sats": limits.window_cap_sats.saturating_sub(t.withdrawn_in_window_sats),
        },
        "held_sats": t.held_sats,
        "withdrawn_sats": t.withdrawn_sats,
        // The most a withdrawal's routing fee may be: held back while it is paid, only what it cost is kept.
        "fee_allowance": { "percent": 1, "min_sats": escrow_core::payout::fee_limit(0) },
        "offramp": st.ramp.as_ref().is_some_and(|r| r.offers_offramp()),
    })))
}

pub async fn withdrawal_list<A, S>(State(st): State<AppState<A, S>>, Auth(user): Auth) -> Result<Json<Value>, ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    let list: Vec<Value> = payouts(&st)?.list(&user.id).await?.iter().map(withdrawal_view).collect();
    Ok(Json(json!({ "withdrawals": list })))
}

#[derive(Deserialize)]
pub struct WithdrawReq {
    invoice: String,
    pin: Option<String>,
}

/// Pay a Lightning invoice out of the caller's balance. The invoice names the amount.
pub async fn withdrawal_create<A, S>(
    State(st): State<AppState<A, S>>,
    Auth(user): Auth,
    ApiJson(req): ApiJson<WithdrawReq>,
) -> Result<(StatusCode, Json<Value>), ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    no_operator(&user)?;
    let p = payouts(&st)?;
    crate::pin::check_for_withdrawal(&st.users, &user.id, req.pin.as_deref(), (st.now)(), st.config.pin_cooloff_secs).await?;
    let w = p.withdraw(&user.id, &req.invoice, WithdrawalKind::Lightning, (st.now)()).await?;
    Ok((StatusCode::CREATED, Json(withdrawal_view(&w))))
}

pub async fn withdrawal_get<A, S>(
    State(st): State<AppState<A, S>>,
    Auth(user): Auth,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    match payouts(&st)?.get(&id).await? {
        Some(w) if w.user_id == user.id => Ok(Json(withdrawal_view(&w))),
        _ => Err(ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such withdrawal")),
    }
}

/// Whether the caller has a withdrawal PIN, and whether it is locked.
pub async fn pin_get<A, S>(State(st): State<AppState<A, S>>, Auth(user): Auth) -> Result<Json<Value>, ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    let pin = st.users.get_withdrawal_pin(&user.id).await?;
    let now = (st.now)();
    let locked_until = pin.as_ref().and_then(|p| p.locked_until).filter(|&t| t > now);
    // While a new PIN is still cooling off: when it starts to work for withdrawals.
    let usable_from = pin.as_ref().map(|p| p.updated_at.saturating_add(st.config.pin_cooloff_secs)).filter(|&t| t > now);
    Ok(Json(json!({ "set": pin.is_some(), "locked_until": locked_until, "usable_from": usable_from })))
}

#[derive(Deserialize)]
pub struct PinReq {
    pin: String,
    /// Needed to change a PIN that is already set.
    current_pin: Option<String>,
}

/// Set the withdrawal PIN, or change it by giving the current one.
pub async fn pin_set<A, S>(
    State(st): State<AppState<A, S>>,
    Auth(user): Auth,
    ApiJson(req): ApiJson<PinReq>,
) -> Result<Json<Value>, ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    no_operator(&user)?;
    let now = (st.now)();
    let pin = req.pin.trim();
    crate::pin::validate(pin)?;
    if st.users.get_withdrawal_pin(&user.id).await?.is_some() {
        crate::pin::check(&st.users, &user.id, req.current_pin.as_deref(), now).await?;
    }
    let hash = crate::pin::hash(pin.to_string()).await?;
    st.users.set_withdrawal_pin(&user.id, &hash, now).await?;
    let usable_from = now.saturating_add(st.config.pin_cooloff_secs);
    Ok(Json(json!({ "set": true, "locked_until": null, "usable_from": (usable_from > now).then_some(usable_from) })))
}

// --- Naira ramp -------------------------------------------------------------------------------

/// The ramp's currency's current rate, from the server's price source.
async fn ramp_rate<A, S: Store + PayoutStore>(st: &AppState<A, S>, r: &RampService<S>) -> Result<i64, ApiError> {
    match st.rates.rate(&r.config().currency, (st.now)()).await {
        Ok(rate) => Ok(rate.rate_minor_per_btc),
        Err(RateError::Unavailable) => {
            Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "rate_unavailable", "the exchange rate is unavailable, try again shortly"))
        }
        Err(_) => Err(ApiError::new(StatusCode::CONFLICT, "ramp_unavailable", "this server has no exchange rate for naira")),
    }
}

/// Whether paying in naira is offered, on what terms, and to which banks.
pub async fn ramp_info<A, S>(State(st): State<AppState<A, S>>, Auth(_user): Auth) -> Result<Json<Value>, ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    let Some(r) = st.ramp.as_deref() else {
        return Ok(Json(json!({ "enabled": false })));
    };
    let rate = st.rates.rate(&r.config().currency, (st.now)()).await.ok();
    let banks: Vec<Value> = r.provider().banks().iter().map(|b| json!({ "code": b.code, "name": b.name })).collect();
    Ok(Json(json!({
        "enabled": true,
        "provider": r.provider().name(),
        "demo": r.provider().is_demo(),
        "currency": r.config().currency,
        "exponent": fiat::exponent(&r.config().currency),
        "fee_bps": r.config().fee_bps,
        "min_sats": r.config().min_sats,
        "rate_minor_per_btc": rate.as_ref().map(|r| r.rate_minor_per_btc),
        "rate_source": rate.as_ref().map(|r| r.source.clone()),
        "offramp": r.offers_offramp(),
        "banks": banks,
    })))
}

#[derive(Deserialize)]
pub struct ResolveReq {
    bank_code: String,
    account_number: String,
}

/// Look up an account's holder, so people can check the name before any money moves.
pub async fn ramp_resolve<A, S>(
    State(st): State<AppState<A, S>>,
    Auth(_user): Auth,
    ApiJson(req): ApiJson<ResolveReq>,
) -> Result<Json<Value>, ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    let a = ramp(&st)?.provider().resolve_account(req.bank_code.trim(), req.account_number.trim()).await?;
    Ok(Json(json!({
        "bank_code": a.bank_code,
        "bank_name": a.bank_name,
        "account_number": a.account_number,
        "account_name": a.account_name,
    })))
}

#[derive(Deserialize)]
pub struct QuoteReq {
    direction: String,
    amount_sats: u64,
}

pub async fn ramp_quote<A, S>(
    State(st): State<AppState<A, S>>,
    Auth(_user): Auth,
    ApiJson(req): ApiJson<QuoteReq>,
) -> Result<Json<Value>, ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    let r = ramp(&st)?;
    let direction: RampDirection =
        req.direction.parse().map_err(|_| ApiError::bad_request("invalid_direction", "direction is onramp or offramp"))?;
    let q = r.quote(direction, req.amount_sats, ramp_rate(&st, r).await?)?;
    Ok(Json(json!({
        "direction": direction.as_str(),
        "amount_sats": q.amount_sats,
        "currency": r.config().currency,
        "exponent": fiat::exponent(&r.config().currency),
        "fiat_minor": q.fiat_minor,
        "fee_minor": q.fee_minor,
        "rate_minor_per_btc": q.rate_minor_per_btc,
    })))
}

#[derive(Deserialize)]
pub struct BankReq {
    bank_code: String,
    account_number: String,
}

/// The buyer of an escrow will pay it by bank transfer. `bank_code` and `account_number` are where a
/// refund goes if the seller is not paid.
pub async fn onramp_start<A, S>(
    State(st): State<AppState<A, S>>,
    Auth(user): Auth,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<BankReq>,
) -> Result<(StatusCode, Json<Value>), ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    let r = ramp(&st)?;
    let escrow = st.service.get(&id).await?;
    if escrow.buyer_id.as_deref() != Some(user.id.as_str()) {
        return Err(ApiError::not_found());
    }
    let bank = r.provider().resolve_account(req.bank_code.trim(), req.account_number.trim()).await?;
    let rate = ramp_rate(&st, r).await?;
    let order = r.start_onramp(&user.id, &escrow, bank, rate, (st.now)()).await?;
    Ok((StatusCode::CREATED, Json(ramp_view(&order))))
}

/// The newest naira payment for an escrow, for its buyer.
pub async fn onramp_get<A, S>(
    State(st): State<AppState<A, S>>,
    Auth(user): Auth,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    let r = ramp(&st)?;
    let escrow = st.service.get(&id).await?;
    if escrow.buyer_id.as_deref() != Some(user.id.as_str()) {
        return Err(ApiError::not_found());
    }
    let order = r.for_escrow(&id).await?;
    Ok(Json(json!({ "order": order.as_ref().map(ramp_view) })))
}

#[derive(Deserialize)]
pub struct OfframpReq {
    amount_sats: u64,
    bank_code: String,
    account_number: String,
    pin: Option<String>,
}

/// Send part of the caller's balance to their bank account, in naira.
pub async fn offramp_start<A, S>(
    State(st): State<AppState<A, S>>,
    Auth(user): Auth,
    ApiJson(req): ApiJson<OfframpReq>,
) -> Result<(StatusCode, Json<Value>), ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    no_operator(&user)?;
    let r = ramp(&st)?;
    crate::pin::check_for_withdrawal(&st.users, &user.id, req.pin.as_deref(), (st.now)(), st.config.pin_cooloff_secs).await?;
    let bank = r.provider().resolve_account(req.bank_code.trim(), req.account_number.trim()).await?;
    let rate = ramp_rate(&st, r).await?;
    let order = r.start_offramp(&user.id, req.amount_sats, bank, rate, (st.now)()).await?;
    Ok((StatusCode::CREATED, Json(ramp_view(&order))))
}

pub async fn ramp_orders<A, S>(State(st): State<AppState<A, S>>, Auth(user): Auth) -> Result<Json<Value>, ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    let list: Vec<Value> = ramp(&st)?.list(&user.id).await?.iter().map(ramp_view).collect();
    Ok(Json(json!({ "orders": list })))
}

async fn own_order<A, S>(st: &AppState<A, S>, user: &User, id: &str) -> Result<RampOrder, ApiError>
where
    S: Store + PayoutStore,
{
    match ramp(st)?.get(id).await {
        Ok(o) if o.user_id == user.id => Ok(o),
        Ok(_) | Err(RampError::NotFound) => Err(ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such order")),
        Err(e) => Err(e.into()),
    }
}

pub async fn ramp_order_get<A, S>(
    State(st): State<AppState<A, S>>,
    Auth(user): Auth,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    Ok(Json(ramp_view(&own_order(&st, &user, &id).await?)))
}

/// Demo provider only: pretend the buyer's bank transfer has arrived. A real provider tells the
/// server itself, when the money is actually in its account.
pub async fn ramp_demo_deposit<A, S>(
    State(st): State<AppState<A, S>>,
    Auth(user): Auth,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    let r = ramp(&st)?;
    if !r.provider().is_demo() {
        return Err(ApiError::forbidden("only the demo provider takes pretend transfers"));
    }
    let o = own_order(&st, &user, &id).await?;
    if o.direction != RampDirection::On {
        return Err(ApiError::bad_request("invalid_request", "only a naira payment takes a transfer"));
    }
    // The pretend transfer comes from the account the buyer named.
    Ok(Json(ramp_view(&r.confirm_deposit(&o.id, &o.bank, (st.now)()).await?)))
}

/// Demo provider only: the operator funds an escrow in naira on its buyer's behalf, as if the buyer
/// had pressed "Pay in naira" and sent the transfer. Lets a demo be driven from a script that has an
/// invoice but not the buyer's sign-in. `bank_code` and `account_number` are where a refund would go.
pub async fn operator_demo_onramp<A, S>(
    State(st): State<AppState<A, S>>,
    Auth(user): Auth,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<BankReq>,
) -> Result<(StatusCode, Json<Value>), ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    if user.role != Role::Operator {
        return Err(ApiError::forbidden("operator only"));
    }
    let r = ramp(&st)?;
    if !r.provider().is_demo() {
        return Err(ApiError::forbidden("only the demo provider takes pretend transfers"));
    }
    let escrow = st.service.get(&id).await?;
    let buyer = escrow
        .buyer_id
        .clone()
        .ok_or_else(|| ApiError::bad_request("invalid_request", "nobody has accepted this offer yet"))?;
    let bank = r.provider().resolve_account(req.bank_code.trim(), req.account_number.trim()).await?;
    let rate = ramp_rate(&st, r).await?;
    let now = (st.now)();
    let order = r.start_onramp(&buyer, &escrow, bank, rate, now).await?;
    let order = r.confirm_deposit(&order.id, &order.bank, now).await?;
    Ok((StatusCode::CREATED, Json(ramp_view(&order))))
}

impl From<PayoutError> for ApiError {
    fn from(e: PayoutError) -> Self {
        match e {
            PayoutError::Invalid(m) => ApiError::bad_request("invalid_invoice", m),
            e @ PayoutError::Insufficient { .. } => ApiError::new(StatusCode::CONFLICT, "insufficient_balance", e.to_string()),
            e @ PayoutError::Duplicate => ApiError::new(StatusCode::CONFLICT, "duplicate_invoice", e.to_string()),
            e @ PayoutError::OverLimit { .. } => ApiError::new(StatusCode::CONFLICT, "withdrawal_limit", e.to_string()),
            PayoutError::Disabled => payouts_unavailable(),
            PayoutError::Adapter(a) => a.into(),
            PayoutError::Store(s) => s.into(),
        }
    }
}

impl From<RampError> for ApiError {
    fn from(e: RampError) -> Self {
        match e {
            RampError::Invalid(m) => ApiError::bad_request("invalid_request", m),
            RampError::NotFound => ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such order"),
            RampError::Unavailable(_) => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "provider_unavailable",
                "the payment provider is unavailable, try again",
            ),
            e @ RampError::Rejected(_) => ApiError::new(StatusCode::BAD_GATEWAY, "provider_rejected", e.to_string()),
            RampError::Payout(p) => p.into(),
            RampError::Store(s) => s.into(),
        }
    }
}
