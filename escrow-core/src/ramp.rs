//! Paying and getting paid in local currency (naira) through a ramp provider.
//!
//! **On-ramp:** a buyer who has no bitcoin pays an escrow by bank transfer. The provider receives the
//! naira and pays the escrow's hold invoice from its own Lightning funds. The escrow itself is
//! unchanged: the payment is still locked until release, and refunded at the deadline. If the seller
//! is not paid, the hold is cancelled, the sats return to the provider, and the provider sends the
//! naira back to the buyer's bank account.
//!
//! **Off-ramp:** a seller sends sats from their balance to the provider's invoice (an ordinary
//! withdrawal), and once that payment has gone through, the provider pays naira into their bank.
//!
//! Whether the provider's payment went through is what its node says, not the escrow's state alone:
//! a buyer could also have paid the invoice another way, and then the naira must go back.

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::{
    adapter::AdapterError,
    fiat::FiatQuote,
    model::{
        BankAccount, DepositInstructions, Escrow, PaymentHash, RampDirection, RampOrder, RampState, WithdrawalKind,
        WithdrawalState,
    },
    payout::{fee_limit, PaymentStatus, PayoutError, PayoutService, Wallet},
    state::EscrowState,
    store::{PayoutStore, RampUpdate, Store, StoreError},
};

#[derive(Debug, Error)]
pub enum RampError {
    #[error("{0}")]
    Invalid(String),
    #[error("no such order")]
    NotFound,
    /// The provider could not be reached or could not act just now; try again.
    #[error("the payment provider is unavailable: {0}")]
    Unavailable(String),
    /// The provider refused.
    #[error("the payment provider refused: {0}")]
    Rejected(String),
    #[error(transparent)]
    Payout(#[from] PayoutError),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

impl From<AdapterError> for RampError {
    fn from(e: AdapterError) -> Self {
        match e {
            AdapterError::Rejected(m) => RampError::Rejected(m),
            other => RampError::Unavailable(other.to_string()),
        }
    }
}

/// A bank the provider can pay into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bank {
    pub code: &'static str,
    pub name: &'static str,
}

/// Why naira is being sent out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FiatPurpose {
    /// An off-ramp: the seller's sats have arrived.
    Payout,
    /// An on-ramp whose escrow did not pay the seller: the buyer's money goes back.
    Refund,
}

impl FiatPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Payout => "payout",
            Self::Refund => "refund",
        }
    }
}

/// A company that turns bank money into sats and back. Real ones (Bitnob, Yellow Card, Busha, ...)
/// need their own API keys and identity checks; [`DemoRamp`] stands in on regtest.
#[async_trait]
pub trait RampProvider: Send + Sync {
    /// Shown to people beside every amount, for example "demo".
    fn name(&self) -> &'static str;
    /// A demo provider moves no real money, so the app may let people pretend a transfer arrived.
    fn is_demo(&self) -> bool;
    fn banks(&self) -> &'static [Bank];
    /// Look up who holds an account (a name enquiry), so people can check before money moves.
    async fn resolve_account(&self, bank_code: &str, account_number: &str) -> Result<BankAccount, RampError>;
    /// Where the buyer sends money for `order`, valid until `expires_at`.
    async fn open_deposit(&self, order: &RampOrder, expires_at: i64) -> Result<DepositInstructions, RampError>;
    /// The deposit arrived: pay the escrow's invoice from the provider's own Lightning funds.
    async fn pay_invoice(&self, order: &RampOrder, invoice: &str) -> Result<(), RampError>;
    /// How the provider's payment of an escrow's invoice stands.
    async fn funding_status(&self, order: &RampOrder, hash: &PaymentHash) -> Result<PaymentStatus, RampError>;
    /// An invoice for the provider to receive an off-ramp's sats.
    async fn receive_invoice(&self, order: &RampOrder) -> Result<String, RampError>;
    /// Send `amount_minor` to `order.bank`. Must be idempotent per order and purpose: a retry after a
    /// crash must not pay twice. Returns the transfer's reference.
    async fn send_fiat(&self, order: &RampOrder, amount_minor: i64, purpose: FiatPurpose) -> Result<String, RampError>;
}

#[derive(Debug, Clone)]
pub struct RampConfig {
    /// ISO 4217 code of the currency the ramp works in.
    pub currency: String,
    /// The provider's fee, in hundredths of a percent of the amount (150 = 1.5%).
    pub fee_bps: u32,
    /// The smallest conversion, in sats.
    pub min_sats: u64,
}

impl Default for RampConfig {
    fn default() -> Self {
        Self { currency: "NGN".into(), fee_bps: 150, min_sats: 1_000 }
    }
}

/// What a conversion comes to, in the currency's minor units (kobo).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RampQuote {
    pub amount_sats: u64,
    /// What the person pays (on-ramp) or receives (off-ramp), the fee included.
    pub fiat_minor: i64,
    pub fee_minor: i64,
    pub rate_minor_per_btc: i64,
}

/// A deposit must have at least this long to arrive, or it is not worth asking for.
const MIN_DEPOSIT_WINDOW_SECS: i64 = 120;
/// The deposit closes this long before the escrow's invoice expires, so the provider can still pay it.
const DEPOSIT_MARGIN_SECS: i64 = 60;
/// How long a payment the provider's node has never heard of is given before it counts as failed.
const UNKNOWN_GRACE_SECS: i64 = 60;

pub fn quote(direction: RampDirection, amount_sats: u64, currency: &str, rate: i64, fee_bps: u32) -> Result<RampQuote, RampError> {
    let base = FiatQuote::new(amount_sats, currency, rate, 0, "")
        .map_err(|e| RampError::Invalid(e.to_string()))?
        .amount_minor;
    // The fee is rounded up, in the provider's favour, like the bank charges it mirrors.
    let fee = (base as i128 * fee_bps as i128 + 9_999) / 10_000;
    let fee = i64::try_from(fee).map_err(|_| RampError::Invalid("amount too large".into()))?;
    let fiat_minor = match direction {
        RampDirection::On => base + fee,
        RampDirection::Off => base - fee,
    };
    if fiat_minor <= 0 {
        return Err(RampError::Invalid("the amount is too small to convert".into()));
    }
    Ok(RampQuote { amount_sats, fiat_minor, fee_minor: fee, rate_minor_per_btc: rate })
}

#[derive(Debug, Default)]
pub struct RampTickReport {
    pub advanced: Vec<(String, RampState)>,
    pub errors: Vec<(String, String)>,
}

pub struct RampService<S> {
    provider: Arc<dyn RampProvider>,
    payouts: Option<Arc<PayoutService<S>>>,
    store: S,
    config: RampConfig,
}

impl<S: Store + PayoutStore> RampService<S> {
    /// Without `payouts` (no shared node to pay from) only the on-ramp is offered.
    pub fn new(provider: Arc<dyn RampProvider>, payouts: Option<Arc<PayoutService<S>>>, store: S, config: RampConfig) -> Self {
        Self { provider, payouts, store, config }
    }

    pub fn provider(&self) -> &dyn RampProvider {
        self.provider.as_ref()
    }

    pub fn config(&self) -> &RampConfig {
        &self.config
    }

    pub fn offers_offramp(&self) -> bool {
        self.payouts.is_some()
    }

    pub fn quote(&self, direction: RampDirection, amount_sats: u64, rate: i64) -> Result<RampQuote, RampError> {
        if amount_sats < self.config.min_sats {
            return Err(RampError::Invalid(format!("the smallest conversion is {} sats", self.config.min_sats)));
        }
        quote(direction, amount_sats, &self.config.currency, rate, self.config.fee_bps)
    }

    pub async fn get(&self, id: &str) -> Result<RampOrder, RampError> {
        self.store.get_ramp_order(id).await?.ok_or(RampError::NotFound)
    }

    pub async fn list(&self, user_id: &str) -> Result<Vec<RampOrder>, RampError> {
        Ok(self.store.list_ramp_orders(user_id).await?)
    }

    pub async fn for_escrow(&self, escrow_id: &str) -> Result<Option<RampOrder>, RampError> {
        Ok(self.store.ramp_order_for_escrow(escrow_id).await?)
    }

    fn check_bank(&self, bank: &BankAccount) -> Result<(), RampError> {
        if !self.provider.banks().iter().any(|b| b.code == bank.bank_code) {
            return Err(RampError::Invalid("choose a bank from the list".into()));
        }
        if bank.account_number.len() != 10 || !bank.account_number.bytes().all(|b| b.is_ascii_digit()) {
            return Err(RampError::Invalid("an account number is 10 digits".into()));
        }
        Ok(())
    }

    fn new_order(&self, user_id: &str, direction: RampDirection, q: RampQuote, bank: BankAccount, now: i64) -> RampOrder {
        RampOrder {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            direction,
            escrow_id: None,
            withdrawal_id: None,
            amount_sats: q.amount_sats,
            currency: self.config.currency.clone(),
            fiat_minor: q.fiat_minor,
            fee_minor: q.fee_minor,
            rate_minor_per_btc: q.rate_minor_per_btc,
            bank,
            deposit: None,
            provider: self.provider.name().to_string(),
            fiat_reference: None,
            state: RampState::AwaitingDeposit,
            note: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// The buyer of `escrow` will pay it by bank transfer. `bank` is where a refund goes. Asking again
    /// while an order is in progress returns that order.
    pub async fn start_onramp(
        &self,
        user_id: &str,
        escrow: &Escrow,
        bank: BankAccount,
        rate: i64,
        now: i64,
    ) -> Result<RampOrder, RampError> {
        if escrow.buyer_id.as_deref() != Some(user_id) {
            return Err(RampError::Invalid("only the buyer can pay for this escrow".into()));
        }
        if let Some(existing) = self.store.ramp_order_for_escrow(&escrow.id).await? {
            if !existing.state.is_terminal() {
                return Ok(existing);
            }
        }
        let (EscrowState::InvoiceIssued, Some(_), Some(invoice_expires_at)) =
            (escrow.state, &escrow.invoice, escrow.invoice_expires_at)
        else {
            return Err(RampError::Invalid("accept the offer first, so there is an invoice to pay".into()));
        };
        let expires_at = invoice_expires_at - DEPOSIT_MARGIN_SECS;
        if expires_at - now < MIN_DEPOSIT_WINDOW_SECS {
            return Err(RampError::Invalid("this invoice expires too soon for a bank transfer".into()));
        }
        self.check_bank(&bank)?;
        let q = self.quote(RampDirection::On, escrow.amount_sats, rate)?;
        let mut order = self.new_order(user_id, RampDirection::On, q, bank, now);
        order.escrow_id = Some(escrow.id.clone());
        order.deposit = Some(self.provider.open_deposit(&order, expires_at).await?);
        match self.store.insert_ramp_order(order.clone()).await {
            Ok(()) => Ok(order),
            // Two requests at once: the other one's order stands.
            Err(StoreError::Conflict) => {
                self.store.ramp_order_for_escrow(&escrow.id).await?.ok_or(RampError::Store(StoreError::Conflict))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The provider says the buyer's transfer has arrived, from `payer`. Saying it again changes nothing.
    ///
    /// A refund always goes back to the account the money came from, never to one the buyer typed:
    /// otherwise money paid from a stolen account could be "refunded" clean into another.
    pub async fn confirm_deposit(&self, order_id: &str, payer: &BankAccount, now: i64) -> Result<RampOrder, RampError> {
        let o = self.get(order_id).await?;
        if o.state != RampState::AwaitingDeposit {
            return Ok(o);
        }
        if o.deposit.as_ref().is_some_and(|d| now >= d.expires_at) {
            return Err(RampError::Invalid("this transfer window has closed".into()));
        }
        self.check_bank(payer)?;
        let update = RampUpdate {
            note: Some("transfer received".into()),
            refund_to: Some(payer.clone()),
            ..Default::default()
        };
        self.store.update_ramp_order(&o.id, RampState::AwaitingDeposit, RampState::DepositReceived, update, now).await?;
        self.get(order_id).await
    }

    /// Send `amount_sats` of `user_id`'s balance to their bank account.
    pub async fn start_offramp(
        &self,
        user_id: &str,
        amount_sats: u64,
        bank: BankAccount,
        rate: i64,
        now: i64,
    ) -> Result<RampOrder, RampError> {
        let payouts = self.payouts.as_ref().ok_or(PayoutError::Disabled)?;
        self.check_bank(&bank)?;
        let q = self.quote(RampDirection::Off, amount_sats, rate)?;
        let mut order = self.new_order(user_id, RampDirection::Off, q, bank, now);
        order.state = RampState::SendingSats;
        let invoice = self.provider.receive_invoice(&order).await?;
        // The withdrawal checks and holds the balance. Only once it exists is the order recorded.
        let w = payouts.withdraw(user_id, &invoice, WithdrawalKind::Bank, now).await?;
        order.withdrawal_id = Some(w.id);
        self.store.insert_ramp_order(order.clone()).await?;
        Ok(order)
    }

    /// Move every open order on as far as it can go.
    pub async fn tick(&self, now: i64) -> Result<RampTickReport, RampError> {
        let mut report = RampTickReport::default();
        for o in self.store.open_ramp_orders().await? {
            match self.advance(&o, now).await {
                Ok(Some((to, update))) => match self.store.update_ramp_order(&o.id, o.state, to, update, now).await {
                    Ok(true) => report.advanced.push((o.id, to)),
                    Ok(false) => {}
                    Err(e) => report.errors.push((o.id, e.to_string())),
                },
                Ok(None) => {}
                Err(e) => report.errors.push((o.id, e.to_string())),
            }
        }
        Ok(report)
    }

    async fn escrow_of(&self, o: &RampOrder) -> Result<Escrow, RampError> {
        let id = o.escrow_id.as_deref().ok_or_else(|| RampError::Invalid("on-ramp order without an escrow".into()))?;
        self.store.get(id).await?.ok_or(RampError::NotFound)
    }

    /// The next state for `o`, if it should move.
    async fn advance(&self, o: &RampOrder, now: i64) -> Result<Option<(RampState, RampUpdate)>, RampError> {
        let note = |n: &str| RampUpdate { note: Some(n.to_string()), ..Default::default() };
        let refund = |why: &str| Ok(Some((RampState::Refunding, note(why))));
        match o.state {
            RampState::AwaitingDeposit => {
                let e = self.escrow_of(o).await?;
                let closed = o.deposit.as_ref().is_some_and(|d| now >= d.expires_at);
                if closed || e.state != EscrowState::InvoiceIssued {
                    return Ok(Some((RampState::Expired, note("no transfer arrived in time"))));
                }
                Ok(None)
            }
            RampState::DepositReceived => {
                let e = self.escrow_of(o).await?;
                let payable = e.state == EscrowState::InvoiceIssued && e.invoice_expires_at.is_some_and(|t| now < t);
                let Some(invoice) = e.invoice.as_deref().filter(|_| payable) else {
                    return refund("the escrow could no longer be paid");
                };
                match self.provider.pay_invoice(o, invoice).await {
                    Ok(()) => Ok(Some((RampState::Paying, note("paying the escrow")))),
                    // Try again on the next pass, while the invoice lasts.
                    Err(RampError::Unavailable(_)) => Ok(None),
                    Err(e) => refund(&format!("the escrow could not be paid: {e}")),
                }
            }
            RampState::Paying | RampState::Funded => {
                let e = self.escrow_of(o).await?;
                let hash = e.payment_hash.ok_or_else(|| RampError::Invalid("escrow without a payment hash".into()))?;
                match self.provider.funding_status(o, &hash).await? {
                    PaymentStatus::Succeeded { .. } => Ok(Some((RampState::Complete, note("the seller was paid")))),
                    PaymentStatus::Failed(why) => refund(&format!("the escrow did not pay the seller ({why})")),
                    PaymentStatus::InFlight
                        if o.state == RampState::Paying && matches!(e.state, EscrowState::Locked | EscrowState::Disputed) =>
                    {
                        Ok(Some((RampState::Funded, note("payment locked in escrow"))))
                    }
                    PaymentStatus::Unknown if now - o.updated_at >= UNKNOWN_GRACE_SECS => {
                        refund("the payment never started")
                    }
                    _ => Ok(None),
                }
            }
            RampState::Refunding => match self.provider.send_fiat(o, o.fiat_minor, FiatPurpose::Refund).await {
                Ok(reference) => Ok(Some((
                    RampState::Refunded,
                    RampUpdate { note: Some("money sent back to your bank".into()), fiat_reference: Some(reference), refund_to: None },
                ))),
                // A refund owed is never dropped: it is retried until the provider takes it.
                Err(_) => Ok(None),
            },
            RampState::SendingSats => {
                let payouts = self.payouts.as_ref().ok_or(PayoutError::Disabled)?;
                let id = o.withdrawal_id.as_deref().ok_or_else(|| RampError::Invalid("off-ramp without a withdrawal".into()))?;
                let w = payouts.get(id).await?.ok_or(RampError::NotFound)?;
                match w.state {
                    WithdrawalState::Pending => Ok(None),
                    WithdrawalState::Succeeded => Ok(Some((RampState::PayingOut, note("sending to your bank")))),
                    WithdrawalState::Failed => Ok(Some((
                        RampState::Failed,
                        note(&format!("the sats could not be sent, so they are back in your balance ({})", w.failure.unwrap_or_default())),
                    ))),
                }
            }
            RampState::PayingOut => match self.provider.send_fiat(o, o.fiat_minor, FiatPurpose::Payout).await {
                Ok(reference) => Ok(Some((
                    RampState::PaidOut,
                    RampUpdate { note: Some("paid into your bank".into()), fiat_reference: Some(reference), refund_to: None },
                ))),
                // A payout owed is never dropped: it is retried until the provider takes it.
                Err(_) => Ok(None),
            },
            _ => Ok(None),
        }
    }
}

/// Nigerian banks and their NIP codes, for the demo provider's list.
pub const NIGERIAN_BANKS: &[Bank] = &[
    Bank { code: "044", name: "Access Bank" },
    Bank { code: "023", name: "Citibank Nigeria" },
    Bank { code: "050", name: "Ecobank Nigeria" },
    Bank { code: "070", name: "Fidelity Bank" },
    Bank { code: "011", name: "First Bank of Nigeria" },
    Bank { code: "214", name: "First City Monument Bank" },
    Bank { code: "058", name: "Guaranty Trust Bank" },
    Bank { code: "030", name: "Heritage Bank" },
    Bank { code: "301", name: "Jaiz Bank" },
    Bank { code: "082", name: "Keystone Bank" },
    Bank { code: "50211", name: "Kuda Microfinance Bank" },
    Bank { code: "50515", name: "Moniepoint Microfinance Bank" },
    Bank { code: "999992", name: "OPay" },
    Bank { code: "999991", name: "PalmPay" },
    Bank { code: "076", name: "Polaris Bank" },
    Bank { code: "221", name: "Stanbic IBTC Bank" },
    Bank { code: "232", name: "Sterling Bank" },
    Bank { code: "032", name: "Union Bank of Nigeria" },
    Bank { code: "033", name: "United Bank for Africa" },
    Bank { code: "215", name: "Unity Bank" },
    Bank { code: "035", name: "Wema Bank" },
    Bank { code: "057", name: "Zenith Bank" },
];

/// A stand-in provider for regtest. Its Lightning side is real (a node of the test network pays and
/// receives); its bank side is pretend: transfers "arrive" when the app says so, and payouts and
/// refunds succeed at once with a made-up reference. Everything it shows is labelled as a demo.
pub struct DemoRamp {
    wallet: Arc<dyn Wallet>,
}

impl DemoRamp {
    /// `wallet` is the node that plays the provider's Lightning funds.
    pub fn new(wallet: Arc<dyn Wallet>) -> Self {
        Self { wallet }
    }
}

fn short(id: &str) -> String {
    id.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect::<String>().to_uppercase()
}

#[async_trait]
impl RampProvider for DemoRamp {
    fn name(&self) -> &'static str {
        "demo"
    }

    fn is_demo(&self) -> bool {
        true
    }

    fn banks(&self) -> &'static [Bank] {
        NIGERIAN_BANKS
    }

    async fn resolve_account(&self, bank_code: &str, account_number: &str) -> Result<BankAccount, RampError> {
        let bank = NIGERIAN_BANKS
            .iter()
            .find(|b| b.code == bank_code)
            .ok_or_else(|| RampError::Invalid("choose a bank from the list".into()))?;
        if account_number.len() != 10 || !account_number.bytes().all(|b| b.is_ascii_digit()) {
            return Err(RampError::Invalid("an account number is 10 digits".into()));
        }
        Ok(BankAccount {
            bank_code: bank.code.to_string(),
            bank_name: bank.name.to_string(),
            account_number: account_number.to_string(),
            account_name: format!("DEMO ACCOUNT {}", &account_number[6..]),
        })
    }

    async fn open_deposit(&self, order: &RampOrder, expires_at: i64) -> Result<DepositInstructions, RampError> {
        let digits: String = order.id.bytes().filter(u8::is_ascii_hexdigit).take(8).map(|b| char::from(b'0' + b % 10)).collect();
        Ok(DepositInstructions {
            bank_name: "Demo Bank (no real money)".into(),
            account_number: format!("99{digits:0<8}"),
            account_name: "TOSHI RAMP DEMO".into(),
            reference: format!("TW-{}", short(&order.id)),
            expires_at,
        })
    }

    async fn pay_invoice(&self, order: &RampOrder, invoice: &str) -> Result<(), RampError> {
        match self.wallet.pay(invoice, fee_limit(order.amount_sats)).await? {
            PaymentStatus::Failed(why) => Err(RampError::Rejected(why)),
            _ => Ok(()),
        }
    }

    async fn funding_status(&self, _order: &RampOrder, hash: &PaymentHash) -> Result<PaymentStatus, RampError> {
        Ok(self.wallet.payment_status(hash).await?)
    }

    async fn receive_invoice(&self, order: &RampOrder) -> Result<String, RampError> {
        Ok(self.wallet.create_invoice(order.amount_sats, &format!("toshi ramp {}", order.id), 3600).await?)
    }

    async fn send_fiat(&self, order: &RampOrder, _amount_minor: i64, purpose: FiatPurpose) -> Result<String, RampError> {
        Ok(format!("DEMO-{}-{}", purpose.as_str().to_uppercase(), short(&order.id)))
    }
}
