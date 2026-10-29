//! Sellers' balances, and paying them out.
//!
//! With the shared node, a release pays the operator, who then owes the sats to the seller. That debt
//! is the seller's balance: the sats of their settled shared-node escrows, less what they have
//! withdrawn. It is worked out from the records every time, never kept as a running number, so it
//! cannot drift from the escrows it comes from.
//!
//! A withdrawal holds back its amount (and the most its fee can be) in the same step that checks the
//! balance, then asks the node to pay. Whether it went through is what the node says about the payment.

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::{
    adapter::AdapterError,
    model::{PaymentHash, Withdrawal, WithdrawalKind, WithdrawalState},
    store::{LedgerTotals, PayoutStore, StoreError, WithdrawalLimits},
};

/// What a node makes of a BOLT11 invoice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedInvoice {
    pub payment_hash: PaymentHash,
    /// `None` for an invoice that leaves the amount to the payer.
    pub amount_sats: Option<u64>,
    pub expires_at: i64,
}

/// A payment as the paying node sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaymentStatus {
    InFlight,
    Succeeded { fee_sats: u64 },
    Failed(String),
    /// The node has no record of it.
    Unknown,
}

/// A node that can pay invoices and ask for payments. Calls carry their own timeouts.
#[async_trait]
pub trait Wallet: Send + Sync {
    async fn decode(&self, invoice: &str) -> Result<DecodedInvoice, AdapterError>;
    /// Start paying `invoice`. Returns once the node has taken the payment on (usually `InFlight`) or
    /// refused it. A payment to a hold invoice stays in flight until the receiver settles it.
    async fn pay(&self, invoice: &str, max_fee_sats: u64) -> Result<PaymentStatus, AdapterError>;
    async fn payment_status(&self, hash: &PaymentHash) -> Result<PaymentStatus, AdapterError>;
    /// An ordinary invoice for this node to be paid `amount_sats`.
    async fn create_invoice(&self, amount_sats: u64, memo: &str, expiry_secs: i64) -> Result<String, AdapterError>;
}

#[derive(Debug, Error)]
pub enum PayoutError {
    #[error("{0}")]
    Invalid(String),
    #[error("not enough balance: {available} sats available, {needed} needed (fee allowance included)")]
    Insufficient { available: u64, needed: u64 },
    #[error("this invoice is already being paid or was paid")]
    Duplicate,
    #[error("that would pass the withdrawal limit: {left} sats left for the next {hours} hours")]
    OverLimit { left: u64, hours: i64 },
    #[error("withdrawals are not offered on this server")]
    Disabled,
    #[error("node error: {0}")]
    Adapter(#[from] AdapterError),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

/// The most a withdrawal's routing fee may be: 1% of the amount, and never less than 10 sats so that
/// small payouts can still find a route.
pub fn fee_limit(amount_sats: u64) -> u64 {
    (amount_sats / 100).max(10)
}

/// How long a pending payment the node has never heard of is given before it counts as failed. Covers
/// the gap between recording a withdrawal and the node taking the payment on.
const UNKNOWN_GRACE_SECS: i64 = 60;

/// An invoice must stay payable at least this long, or it could expire while the node routes it.
const MIN_INVOICE_LIFE_SECS: i64 = 30;

/// Accept an invoice as people paste it: trimmed, any case, with or without `lightning:`.
pub fn normalize_invoice(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    s.strip_prefix("lightning:").unwrap_or(&s).to_string()
}

#[derive(Debug, Default)]
pub struct PayoutSyncReport {
    pub finished: Vec<String>,
    pub errors: Vec<(String, String)>,
}

/// How fast money may leave. Lightning payments cannot be undone, so these slow down someone cashing
/// out money that can still be reversed, or a balance in the hands of whoever stole a sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayoutLimits {
    /// Sats from an escrow paid in naira wait this long after settling before they can be withdrawn,
    /// the time in which a bank transfer can still be reversed.
    pub naira_hold_secs: i64,
    /// The most a seller may withdraw in any `window_secs`.
    pub window_cap_sats: u64,
    pub window_secs: i64,
}

impl Default for PayoutLimits {
    fn default() -> Self {
        Self { naira_hold_secs: 24 * 3600, window_cap_sats: 1_000_000, window_secs: 24 * 3600 }
    }
}

impl PayoutLimits {
    pub fn at(&self, now: i64) -> WithdrawalLimits {
        WithdrawalLimits {
            matured_before: now - self.naira_hold_secs,
            window_from: now - self.window_secs,
            window_cap_sats: self.window_cap_sats,
        }
    }
}

pub struct PayoutService<S> {
    wallet: Arc<dyn Wallet>,
    store: S,
    limits: PayoutLimits,
}

impl<S: PayoutStore> PayoutService<S> {
    /// `wallet` is the node the owed sats sit on: the service's shared node.
    pub fn new(wallet: Arc<dyn Wallet>, store: S) -> Self {
        Self { wallet, store, limits: PayoutLimits::default() }
    }

    pub fn with_limits(mut self, limits: PayoutLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn limits(&self) -> PayoutLimits {
        self.limits
    }

    pub async fn balance(&self, user_id: &str, now: i64) -> Result<LedgerTotals, PayoutError> {
        Ok(self.store.ledger_totals(user_id, &self.limits.at(now)).await?)
    }

    pub async fn list(&self, user_id: &str) -> Result<Vec<Withdrawal>, PayoutError> {
        Ok(self.store.list_withdrawals(user_id).await?)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Withdrawal>, PayoutError> {
        Ok(self.store.get_withdrawal(id).await?)
    }

    /// Pay `invoice` out of `user_id`'s balance. The invoice must name its amount.
    pub async fn withdraw(
        &self,
        user_id: &str,
        invoice: &str,
        kind: WithdrawalKind,
        now: i64,
    ) -> Result<Withdrawal, PayoutError> {
        let invoice = normalize_invoice(invoice);
        if !invoice.starts_with("ln") || invoice.len() > 2048 {
            return Err(PayoutError::Invalid("that is not a Lightning invoice".into()));
        }
        let decoded = match self.wallet.decode(&invoice).await {
            Ok(d) => d,
            Err(AdapterError::Rejected(_)) => {
                return Err(PayoutError::Invalid("that invoice could not be read; check it is for this network".into()))
            }
            Err(e) => return Err(e.into()),
        };
        let amount = match decoded.amount_sats {
            Some(a) if a > 0 => a,
            _ => return Err(PayoutError::Invalid("the invoice must say how many sats to send".into())),
        };
        if decoded.expires_at < now + MIN_INVOICE_LIFE_SECS {
            return Err(PayoutError::Invalid("that invoice has expired; make a new one".into()));
        }
        let w = Withdrawal {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            kind,
            amount_sats: amount,
            fee_limit_sats: fee_limit(amount),
            fee_sats: None,
            invoice: invoice.clone(),
            payment_hash: decoded.payment_hash,
            state: WithdrawalState::Pending,
            failure: None,
            created_at: now,
            updated_at: now,
        };
        let limits = self.limits.at(now);
        match self.store.insert_withdrawal(w.clone(), &limits).await {
            Ok(true) => {}
            Ok(false) => {
                let t = self.store.ledger_totals(user_id, &limits).await?;
                if t.withdrawn_in_window_sats + amount > limits.window_cap_sats {
                    let left = limits.window_cap_sats.saturating_sub(t.withdrawn_in_window_sats);
                    return Err(PayoutError::OverLimit { left, hours: self.limits.window_secs / 3600 });
                }
                return Err(PayoutError::Insufficient { available: t.available(), needed: w.debit() });
            }
            Err(StoreError::Conflict) => return Err(PayoutError::Duplicate),
            Err(e) => return Err(e.into()),
        }
        // The sats are held from here on. If the node cannot be reached the withdrawal stays pending
        // and `sync` settles it from what the node later says, so nothing is paid twice or lost.
        match self.wallet.pay(&invoice, w.fee_limit_sats).await {
            Ok(status) => {
                self.apply(&w, status, now).await?;
            }
            Err(AdapterError::Rejected(reason)) => {
                self.apply(&w, PaymentStatus::Failed(reason), now).await?;
            }
            Err(_) => {}
        }
        Ok(self.store.get_withdrawal(&w.id).await?.unwrap_or(w))
    }

    /// Bring every pending withdrawal up to date with the node.
    pub async fn sync(&self, now: i64) -> Result<PayoutSyncReport, PayoutError> {
        let mut report = PayoutSyncReport::default();
        for w in self.store.pending_withdrawals().await? {
            match self.wallet.payment_status(&w.payment_hash).await {
                Ok(status) => match self.apply(&w, status, now).await {
                    Ok(true) => report.finished.push(w.id),
                    Ok(false) => {}
                    Err(e) => report.errors.push((w.id, e.to_string())),
                },
                Err(e) => report.errors.push((w.id, e.to_string())),
            }
        }
        Ok(report)
    }

    /// Record what the node says. `true` if the withdrawal finished now.
    async fn apply(&self, w: &Withdrawal, status: PaymentStatus, now: i64) -> Result<bool, PayoutError> {
        let (state, fee, failure) = match status {
            PaymentStatus::InFlight => return Ok(false),
            PaymentStatus::Unknown if now - w.created_at < UNKNOWN_GRACE_SECS => return Ok(false),
            PaymentStatus::Unknown => (WithdrawalState::Failed, None, Some("the node never started this payment".into())),
            PaymentStatus::Succeeded { fee_sats } => (WithdrawalState::Succeeded, Some(fee_sats), None),
            PaymentStatus::Failed(reason) => (WithdrawalState::Failed, None, Some(reason)),
        };
        Ok(self.store.finish_withdrawal(&w.id, state, fee, failure, now).await?)
    }
}
