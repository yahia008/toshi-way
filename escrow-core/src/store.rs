//! Persistence boundary. State changes are compare-and-set so concurrent actors (a node event
//! and the scheduler, say) cannot both win, and each change is written together with its audit entry.

use std::sync::Mutex;

use async_trait::async_trait;
use thiserror::Error;

use crate::{
    model::{
        ApiKey, AuditEntry, AuditRow, BankAccount, Escrow, NodeConnection, NodeEventRecord, NodeKind, PaymentHash, RampOrder, RampState,
        User, Webhook, WebhookProgress, Withdrawal, WithdrawalState,
    },
    state::EscrowState,
};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StoreError {
    #[error("escrow not found")]
    NotFound,
    #[error("escrow was changed concurrently")]
    Conflict,
    #[error("storage failure: {0}")]
    Backend(String),
}

/// What a transition changes besides the state itself.
#[derive(Debug, Clone, Default)]
pub struct EscrowUpdate {
    pub arbiter_sealed: Option<String>,
    pub arbiter_confirmed_at: Option<i64>,
    /// Set when the first user to accept an open offer becomes its buyer.
    pub buyer_id: Option<String>,
    pub payment_hash: Option<PaymentHash>,
    pub invoice: Option<String>,
    pub invoice_expires_at: Option<i64>,
}

/// What a user's whole history adds up to. Money counts only where they are the buyer or the seller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UserTotals {
    /// Every escrow they are a party to, in any role and state.
    pub escrows: u64,
    pub settled_count: u64,
    /// Sats of settled escrows where they are the seller.
    pub settled_received: u64,
    /// Sats of settled escrows where they are the buyer.
    pub settled_paid: u64,
    pub refunded_count: u64,
    /// Sats of refunded escrows where they are the buyer or the seller.
    pub refunded_sats: u64,
}

#[async_trait]
pub trait Store: Send + Sync {
    async fn insert(&self, escrow: Escrow) -> Result<(), StoreError>;
    async fn get(&self, id: &str) -> Result<Option<Escrow>, StoreError>;
    async fn get_by_hash(&self, hash: &PaymentHash) -> Result<Option<Escrow>, StoreError>;
    /// Escrows that are not in a terminal state.
    async fn list_active(&self) -> Result<Vec<Escrow>, StoreError>;
    /// Escrows where `user_id` is the buyer, the seller or the arbiter, newest first (ties by id): every
    /// one that is still in play, plus the newest 100 that have ended. `totals_for_user` covers the rest.
    async fn list_for_user(&self, user_id: &str) -> Result<Vec<Escrow>, StoreError>;
    /// Counts and sums over all of `user_id`'s escrows, however old, so totals do not depend on how many
    /// escrows `list_for_user` returns.
    async fn totals_for_user(&self, user_id: &str) -> Result<UserTotals, StoreError>;
    /// Atomically move `id` from `expected` to `audit.to_state`, apply `update`, and append
    /// `audit`. Fails with `Conflict` if the escrow is no longer in `expected`.
    async fn transition(
        &self,
        id: &str,
        expected: EscrowState,
        update: EscrowUpdate,
        audit: AuditEntry,
    ) -> Result<Escrow, StoreError>;
    async fn audit_log(&self, id: &str) -> Result<Vec<AuditEntry>, StoreError>;
    /// Audit entries for escrows sold by `seller_id`, with a position above `after`, oldest first.
    async fn audit_since(&self, seller_id: &str, after: i64, limit: u32) -> Result<Vec<AuditRow>, StoreError>;
    /// The position of the newest audit entry (0 if none), for starting a cursor "from now".
    async fn audit_head(&self) -> Result<i64, StoreError>;
    async fn record_node_event(&self, event: NodeEventRecord) -> Result<(), StoreError>;
    async fn node_events(&self, id: &str) -> Result<Vec<NodeEventRecord>, StoreError>;
}

/// Accounts and their API tokens. Only a hash of the token is ever stored.
#[async_trait]
pub trait UserStore: Send + Sync {
    /// `Conflict` if the id or the token hash already exists.
    async fn insert_user(&self, user: User, token_hash: String) -> Result<(), StoreError>;
    async fn user_by_token_hash(&self, token_hash: &str) -> Result<Option<User>, StoreError>;
    async fn get_user(&self, id: &str) -> Result<Option<User>, StoreError>;
    /// Register (replacing any earlier one) the public key others encrypt release secrets to when
    /// they name this user as their arbiter. Base64 SPKI of an ECDH P-256 key.
    async fn put_arbiter_key(&self, user_id: &str, public_key: &str, at: i64) -> Result<(), StoreError>;
    async fn arbiter_key(&self, user_id: &str) -> Result<Option<String>, StoreError>;
    /// Save (replacing any earlier one) how to reach a seller's own node.
    async fn put_node_connection(&self, conn: NodeConnection) -> Result<(), StoreError>;
    async fn node_connection(&self, user_id: &str) -> Result<Option<NodeConnection>, StoreError>;
    /// `true` if there was one to delete.
    async fn delete_node_connection(&self, user_id: &str) -> Result<bool, StoreError>;
    /// `Conflict` if the id or the key hash already exists.
    async fn insert_api_key(&self, key: ApiKey, key_hash: String) -> Result<(), StoreError>;
    /// The key with this hash, revoked or not; the caller checks `revoked_at`.
    async fn api_key_by_hash(&self, key_hash: &str) -> Result<Option<ApiKey>, StoreError>;
    /// All of a user's keys, newest first, revoked ones included.
    async fn list_api_keys(&self, user_id: &str) -> Result<Vec<ApiKey>, StoreError>;
    /// Revoke `id` if it belongs to `user_id` and is not revoked yet. `true` if it was revoked now.
    async fn revoke_api_key(&self, user_id: &str, id: &str, at: i64) -> Result<bool, StoreError>;
    async fn touch_api_key(&self, id: &str, at: i64) -> Result<(), StoreError>;
    /// `Conflict` if the id already exists.
    async fn insert_webhook(&self, hook: Webhook) -> Result<(), StoreError>;
    async fn get_webhook(&self, id: &str) -> Result<Option<Webhook>, StoreError>;
    /// A user's endpoints, oldest first.
    async fn list_webhooks(&self, user_id: &str) -> Result<Vec<Webhook>, StoreError>;
    /// `true` if `user_id` had endpoint `id` and it was deleted.
    async fn delete_webhook(&self, user_id: &str, id: &str) -> Result<bool, StoreError>;
    /// Endpoints that are not disabled and whose next attempt is due at `now`.
    async fn due_webhooks(&self, now: i64, limit: u32) -> Result<Vec<Webhook>, StoreError>;
    /// Record how a delivery attempt went. Does nothing if the endpoint is gone.
    async fn record_webhook_progress(&self, id: &str, progress: WebhookProgress) -> Result<(), StoreError>;
}

/// The sums behind a seller's balance. Only escrows on the shared node count: with their own node
/// the sats were never the operator's to hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LedgerTotals {
    /// Sats of settled shared-node escrows where they are the seller.
    pub earned_sats: u64,
    /// The part of `earned_sats` paid in naira and settled too recently to withdraw. A bank transfer
    /// can be reversed for a while; sats already sent out over Lightning cannot.
    pub maturing_sats: u64,
    /// Sent or being sent in the current limit window (amounts only, failed ones not counted).
    pub withdrawn_in_window_sats: u64,
    /// Held back for withdrawals still being paid: the amount plus the most the fee can be.
    pub held_sats: u64,
    /// Paid out by withdrawals that went through, fees included.
    pub withdrawn_sats: u64,
}

impl LedgerTotals {
    pub fn available(&self) -> u64 {
        self.earned_sats.saturating_sub(self.maturing_sats + self.held_sats + self.withdrawn_sats)
    }
}

/// The rules a withdrawal is checked against, as of one moment. Part of the same atomic step as the
/// balance check, so two withdrawals at once cannot both slip under a limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WithdrawalLimits {
    /// Naira-paid escrows settled after this time are still maturing.
    pub matured_before: i64,
    /// Withdrawals made from this time on count towards `window_cap_sats`.
    pub window_from: i64,
    pub window_cap_sats: u64,
}

impl WithdrawalLimits {
    /// No holding period and no cap.
    pub const NONE: Self = Self { matured_before: i64::MIN, window_from: i64::MAX, window_cap_sats: i64::MAX as u64 };
}

/// What a ramp order's step changes besides its state.
#[derive(Debug, Clone, Default)]
pub struct RampUpdate {
    pub note: Option<String>,
    pub fiat_reference: Option<String>,
    /// Replaces the order's bank account: where a refund goes, once the provider says which account
    /// the money really came from.
    pub refund_to: Option<BankAccount>,
}

/// Payouts of the sats the operator owes sellers, and conversions to and from local currency.
#[async_trait]
pub trait PayoutStore: Send + Sync {
    async fn ledger_totals(&self, user_id: &str, limits: &WithdrawalLimits) -> Result<LedgerTotals, StoreError>;
    /// Record a new pending withdrawal, but only if the user's available balance covers its
    /// [`Withdrawal::debit`]. The check and the insert are one atomic step, so two withdrawals at
    /// once cannot both spend the same sats. `Ok(false)` if the balance is too low; `Conflict` if
    /// the id is taken or the same invoice is already being paid or was paid.
    async fn insert_withdrawal(&self, w: Withdrawal, limits: &WithdrawalLimits) -> Result<bool, StoreError>;
    async fn get_withdrawal(&self, id: &str) -> Result<Option<Withdrawal>, StoreError>;
    /// A user's newest 100 withdrawals, newest first.
    async fn list_withdrawals(&self, user_id: &str) -> Result<Vec<Withdrawal>, StoreError>;
    async fn pending_withdrawals(&self) -> Result<Vec<Withdrawal>, StoreError>;
    /// Move a pending withdrawal to `state`. `false` if it was no longer pending.
    async fn finish_withdrawal(
        &self,
        id: &str,
        state: WithdrawalState,
        fee_sats: Option<u64>,
        failure: Option<String>,
        at: i64,
    ) -> Result<bool, StoreError>;

    /// `Conflict` if the id is taken, or (for an on-ramp) the escrow already has an order in progress.
    async fn insert_ramp_order(&self, order: RampOrder) -> Result<(), StoreError>;
    async fn get_ramp_order(&self, id: &str) -> Result<Option<RampOrder>, StoreError>;
    /// A user's newest 100 orders, newest first.
    async fn list_ramp_orders(&self, user_id: &str) -> Result<Vec<RampOrder>, StoreError>;
    /// Orders that have not reached a terminal state.
    async fn open_ramp_orders(&self) -> Result<Vec<RampOrder>, StoreError>;
    /// The newest on-ramp order for an escrow, in any state.
    async fn ramp_order_for_escrow(&self, escrow_id: &str) -> Result<Option<RampOrder>, StoreError>;
    /// Compare-and-set: move `id` from `expected` to `to`. `false` if it was no longer in `expected`.
    async fn update_ramp_order(
        &self,
        id: &str,
        expected: RampState,
        to: RampState,
        update: RampUpdate,
        at: i64,
    ) -> Result<bool, StoreError>;

    async fn get_withdrawal_pin(&self, user_id: &str) -> Result<Option<WithdrawalPin>, StoreError>;
    /// Set or replace a user's PIN hash, clearing any wrong guesses and lock.
    async fn set_withdrawal_pin(&self, user_id: &str, pin_hash: &str, at: i64) -> Result<(), StoreError>;
    /// Count a wrong guess, in one step so guesses made at once are all counted. Once there have been
    /// `max_failures`, the PIN is locked until `lock_until`.
    async fn withdrawal_pin_failed(&self, user_id: &str, max_failures: u32, lock_until: i64) -> Result<(), StoreError>;
    /// A right guess: forget the wrong ones.
    async fn withdrawal_pin_ok(&self, user_id: &str) -> Result<(), StoreError>;
}

/// The PIN a seller gives to withdraw: only its hash is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithdrawalPin {
    pub pin_hash: String,
    /// Wrong guesses since the last right one.
    pub failures: u32,
    pub locked_until: Option<i64>,
    pub updated_at: i64,
}

#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<MemoryInner>,
}

#[derive(Default)]
struct MemoryInner {
    escrows: Vec<Escrow>,
    audit: Vec<AuditEntry>,
    node_events: Vec<NodeEventRecord>,
    users: Vec<(User, String)>,
    node_connections: Vec<NodeConnection>,
    arbiter_keys: Vec<(String, String)>,
    api_keys: Vec<(ApiKey, String)>,
    webhooks: Vec<Webhook>,
    withdrawals: Vec<Withdrawal>,
    ramp_orders: Vec<RampOrder>,
    pins: Vec<(String, WithdrawalPin)>,
}

impl MemoryInner {
    fn ledger_totals(&self, user_id: &str, limits: &WithdrawalLimits) -> LedgerTotals {
        let mut t = LedgerTotals::default();
        for e in &self.escrows {
            if e.seller_id == user_id && e.node == NodeKind::Shared && e.state == EscrowState::Settled {
                t.earned_sats += e.amount_sats;
                if e.updated_at > limits.matured_before && self.paid_in_naira(&e.id) {
                    t.maturing_sats += e.amount_sats;
                }
            }
        }
        for w in self.withdrawals.iter().filter(|w| w.user_id == user_id) {
            match w.state {
                WithdrawalState::Pending => t.held_sats += w.debit(),
                WithdrawalState::Succeeded => t.withdrawn_sats += w.debit(),
                WithdrawalState::Failed => {}
            }
            if w.state != WithdrawalState::Failed && w.created_at >= limits.window_from {
                t.withdrawn_in_window_sats += w.amount_sats;
            }
        }
        t
    }

    /// Whether a naira payment funded this escrow.
    fn paid_in_naira(&self, escrow_id: &str) -> bool {
        self.ramp_orders.iter().any(|o| {
            o.escrow_id.as_deref() == Some(escrow_id)
                && matches!(o.state, RampState::Paying | RampState::Funded | RampState::Complete)
        })
    }
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn insert(&self, escrow: Escrow) -> Result<(), StoreError> {
        let mut g = self.inner.lock().unwrap();
        if g.escrows.iter().any(|e| e.id == escrow.id) {
            return Err(StoreError::Conflict);
        }
        g.escrows.push(escrow);
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<Escrow>, StoreError> {
        Ok(self.inner.lock().unwrap().escrows.iter().find(|e| e.id == id).cloned())
    }

    async fn get_by_hash(&self, hash: &PaymentHash) -> Result<Option<Escrow>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .escrows
            .iter()
            .find(|e| e.payment_hash.as_ref() == Some(hash))
            .cloned())
    }

    async fn list_active(&self) -> Result<Vec<Escrow>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .escrows
            .iter()
            .filter(|e| !e.state.is_terminal())
            .cloned()
            .collect())
    }

    async fn list_for_user(&self, user_id: &str) -> Result<Vec<Escrow>, StoreError> {
        let mut list: Vec<Escrow> = self
            .inner
            .lock()
            .unwrap()
            .escrows
            .iter()
            .filter(|e| {
                e.buyer_id.as_deref() == Some(user_id)
                    || e.seller_id == user_id
                    || e.arbiter_id.as_deref() == Some(user_id)
            })
            .cloned()
            .collect();
        list.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.id.cmp(&b.id)));
        let mut ended = 0;
        list.retain(|e| {
            if !e.state.is_terminal() {
                return true;
            }
            ended += 1;
            ended <= 100
        });
        Ok(list)
    }

    async fn totals_for_user(&self, user_id: &str) -> Result<UserTotals, StoreError> {
        let inner = self.inner.lock().unwrap();
        let mut t = UserTotals::default();
        for e in inner.escrows.iter() {
            let buyer = e.buyer_id.as_deref() == Some(user_id);
            let seller = e.seller_id == user_id;
            if !(buyer || seller || e.arbiter_id.as_deref() == Some(user_id)) {
                continue;
            }
            t.escrows += 1;
            match e.state {
                EscrowState::Settled => {
                    t.settled_count += 1;
                    if seller {
                        t.settled_received += e.amount_sats;
                    }
                    if buyer {
                        t.settled_paid += e.amount_sats;
                    }
                }
                EscrowState::Refunded => {
                    t.refunded_count += 1;
                    if buyer || seller {
                        t.refunded_sats += e.amount_sats;
                    }
                }
                _ => {}
            }
        }
        Ok(t)
    }

    async fn transition(
        &self,
        id: &str,
        expected: EscrowState,
        update: EscrowUpdate,
        audit: AuditEntry,
    ) -> Result<Escrow, StoreError> {
        let mut g = self.inner.lock().unwrap();
        let e = g.escrows.iter_mut().find(|e| e.id == id).ok_or(StoreError::NotFound)?;
        if e.state != expected {
            return Err(StoreError::Conflict);
        }
        e.state = audit.to_state;
        e.updated_at = audit.created_at;
        if update.buyer_id.is_some() {
            e.buyer_id = update.buyer_id;
        }
        if update.arbiter_sealed.is_some() {
            e.arbiter_sealed = update.arbiter_sealed;
        }
        if update.arbiter_confirmed_at.is_some() {
            e.arbiter_confirmed_at = update.arbiter_confirmed_at;
        }
        if update.payment_hash.is_some() {
            e.payment_hash = update.payment_hash;
        }
        if update.invoice.is_some() {
            e.invoice = update.invoice;
        }
        if update.invoice_expires_at.is_some() {
            e.invoice_expires_at = update.invoice_expires_at;
        }
        let updated = e.clone();
        g.audit.push(audit);
        Ok(updated)
    }

    async fn audit_log(&self, id: &str) -> Result<Vec<AuditEntry>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .audit
            .iter()
            .filter(|a| a.escrow_id == id)
            .cloned()
            .collect())
    }

    async fn audit_since(&self, seller_id: &str, after: i64, limit: u32) -> Result<Vec<AuditRow>, StoreError> {
        let g = self.inner.lock().unwrap();
        // The log is a Vec, so an entry's position is its index plus one.
        Ok(g.audit
            .iter()
            .enumerate()
            .map(|(i, entry)| AuditRow { seq: i as i64 + 1, entry: entry.clone() })
            .filter(|row| row.seq > after)
            .filter(|row| g.escrows.iter().any(|e| e.id == row.entry.escrow_id && e.seller_id == seller_id))
            .take(limit as usize)
            .collect())
    }

    async fn audit_head(&self) -> Result<i64, StoreError> {
        Ok(self.inner.lock().unwrap().audit.len() as i64)
    }

    async fn record_node_event(&self, event: NodeEventRecord) -> Result<(), StoreError> {
        self.inner.lock().unwrap().node_events.push(event);
        Ok(())
    }

    async fn node_events(&self, id: &str) -> Result<Vec<NodeEventRecord>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .node_events
            .iter()
            .filter(|n| n.escrow_id == id)
            .cloned()
            .collect())
    }
}

/// Lets a service and a "restarted" service share one store.
#[async_trait]
impl<S: Store + ?Sized> Store for std::sync::Arc<S> {
    async fn insert(&self, escrow: Escrow) -> Result<(), StoreError> {
        (**self).insert(escrow).await
    }
    async fn get(&self, id: &str) -> Result<Option<Escrow>, StoreError> {
        (**self).get(id).await
    }
    async fn get_by_hash(&self, hash: &PaymentHash) -> Result<Option<Escrow>, StoreError> {
        (**self).get_by_hash(hash).await
    }
    async fn list_active(&self) -> Result<Vec<Escrow>, StoreError> {
        (**self).list_active().await
    }
    async fn list_for_user(&self, user_id: &str) -> Result<Vec<Escrow>, StoreError> {
        (**self).list_for_user(user_id).await
    }
    async fn totals_for_user(&self, user_id: &str) -> Result<UserTotals, StoreError> {
        (**self).totals_for_user(user_id).await
    }
    async fn transition(
        &self,
        id: &str,
        expected: EscrowState,
        update: EscrowUpdate,
        audit: AuditEntry,
    ) -> Result<Escrow, StoreError> {
        (**self).transition(id, expected, update, audit).await
    }
    async fn audit_log(&self, id: &str) -> Result<Vec<AuditEntry>, StoreError> {
        (**self).audit_log(id).await
    }
    async fn audit_since(&self, seller_id: &str, after: i64, limit: u32) -> Result<Vec<AuditRow>, StoreError> {
        (**self).audit_since(seller_id, after, limit).await
    }
    async fn audit_head(&self) -> Result<i64, StoreError> {
        (**self).audit_head().await
    }
    async fn record_node_event(&self, event: NodeEventRecord) -> Result<(), StoreError> {
        (**self).record_node_event(event).await
    }
    async fn node_events(&self, id: &str) -> Result<Vec<NodeEventRecord>, StoreError> {
        (**self).node_events(id).await
    }
}

#[async_trait]
impl UserStore for MemoryStore {
    async fn insert_user(&self, user: User, token_hash: String) -> Result<(), StoreError> {
        let mut g = self.inner.lock().unwrap();
        if g.users.iter().any(|(u, h)| u.id == user.id || *h == token_hash) {
            return Err(StoreError::Conflict);
        }
        g.users.push((user, token_hash));
        Ok(())
    }

    async fn user_by_token_hash(&self, token_hash: &str) -> Result<Option<User>, StoreError> {
        Ok(self.inner.lock().unwrap().users.iter().find(|(_, h)| h == token_hash).map(|(u, _)| u.clone()))
    }

    async fn get_user(&self, id: &str) -> Result<Option<User>, StoreError> {
        Ok(self.inner.lock().unwrap().users.iter().find(|(u, _)| u.id == id).map(|(u, _)| u.clone()))
    }

    async fn put_arbiter_key(&self, user_id: &str, public_key: &str, _at: i64) -> Result<(), StoreError> {
        let mut g = self.inner.lock().unwrap();
        g.arbiter_keys.retain(|(u, _)| u != user_id);
        g.arbiter_keys.push((user_id.to_string(), public_key.to_string()));
        Ok(())
    }

    async fn arbiter_key(&self, user_id: &str) -> Result<Option<String>, StoreError> {
        Ok(self.inner.lock().unwrap().arbiter_keys.iter().find(|(u, _)| u == user_id).map(|(_, k)| k.clone()))
    }

    async fn put_node_connection(&self, conn: NodeConnection) -> Result<(), StoreError> {
        let mut g = self.inner.lock().unwrap();
        g.node_connections.retain(|c| c.user_id != conn.user_id);
        g.node_connections.push(conn);
        Ok(())
    }

    async fn node_connection(&self, user_id: &str) -> Result<Option<NodeConnection>, StoreError> {
        Ok(self.inner.lock().unwrap().node_connections.iter().find(|c| c.user_id == user_id).cloned())
    }

    async fn delete_node_connection(&self, user_id: &str) -> Result<bool, StoreError> {
        let mut g = self.inner.lock().unwrap();
        let before = g.node_connections.len();
        g.node_connections.retain(|c| c.user_id != user_id);
        Ok(g.node_connections.len() != before)
    }

    async fn insert_api_key(&self, key: ApiKey, key_hash: String) -> Result<(), StoreError> {
        let mut g = self.inner.lock().unwrap();
        if g.api_keys.iter().any(|(k, h)| k.id == key.id || *h == key_hash) {
            return Err(StoreError::Conflict);
        }
        g.api_keys.push((key, key_hash));
        Ok(())
    }

    async fn api_key_by_hash(&self, key_hash: &str) -> Result<Option<ApiKey>, StoreError> {
        Ok(self.inner.lock().unwrap().api_keys.iter().find(|(_, h)| h == key_hash).map(|(k, _)| k.clone()))
    }

    async fn list_api_keys(&self, user_id: &str) -> Result<Vec<ApiKey>, StoreError> {
        let mut list: Vec<ApiKey> =
            self.inner.lock().unwrap().api_keys.iter().filter(|(k, _)| k.user_id == user_id).map(|(k, _)| k.clone()).collect();
        list.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.id.cmp(&b.id)));
        Ok(list)
    }

    async fn revoke_api_key(&self, user_id: &str, id: &str, at: i64) -> Result<bool, StoreError> {
        let mut g = self.inner.lock().unwrap();
        match g.api_keys.iter_mut().find(|(k, _)| k.id == id && k.user_id == user_id && k.revoked_at.is_none()) {
            Some((k, _)) => {
                k.revoked_at = Some(at);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn touch_api_key(&self, id: &str, at: i64) -> Result<(), StoreError> {
        if let Some((k, _)) = self.inner.lock().unwrap().api_keys.iter_mut().find(|(k, _)| k.id == id) {
            k.last_used_at = Some(at);
        }
        Ok(())
    }

    async fn insert_webhook(&self, hook: Webhook) -> Result<(), StoreError> {
        let mut g = self.inner.lock().unwrap();
        if g.webhooks.iter().any(|h| h.id == hook.id) {
            return Err(StoreError::Conflict);
        }
        g.webhooks.push(hook);
        Ok(())
    }

    async fn get_webhook(&self, id: &str) -> Result<Option<Webhook>, StoreError> {
        Ok(self.inner.lock().unwrap().webhooks.iter().find(|h| h.id == id).cloned())
    }

    async fn list_webhooks(&self, user_id: &str) -> Result<Vec<Webhook>, StoreError> {
        let mut list: Vec<Webhook> =
            self.inner.lock().unwrap().webhooks.iter().filter(|h| h.user_id == user_id).cloned().collect();
        list.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        Ok(list)
    }

    async fn delete_webhook(&self, user_id: &str, id: &str) -> Result<bool, StoreError> {
        let mut g = self.inner.lock().unwrap();
        let before = g.webhooks.len();
        g.webhooks.retain(|h| !(h.id == id && h.user_id == user_id));
        Ok(g.webhooks.len() != before)
    }

    async fn due_webhooks(&self, now: i64, limit: u32) -> Result<Vec<Webhook>, StoreError> {
        let mut list: Vec<Webhook> = self
            .inner
            .lock()
            .unwrap()
            .webhooks
            .iter()
            .filter(|h| h.disabled_at.is_none() && h.next_attempt_at <= now)
            .cloned()
            .collect();
        list.sort_by(|a, b| a.next_attempt_at.cmp(&b.next_attempt_at).then_with(|| a.id.cmp(&b.id)));
        list.truncate(limit as usize);
        Ok(list)
    }

    async fn record_webhook_progress(&self, id: &str, p: WebhookProgress) -> Result<(), StoreError> {
        if let Some(h) = self.inner.lock().unwrap().webhooks.iter_mut().find(|h| h.id == id) {
            h.last_seq = p.last_seq;
            h.attempts = p.attempts;
            h.next_attempt_at = p.next_attempt_at;
            h.failing_since = p.failing_since;
            h.disabled_at = p.disabled_at;
            h.last_attempt_at = p.last_attempt_at;
            h.last_status = p.last_status;
        }
        Ok(())
    }
}

#[async_trait]
impl<S: UserStore + ?Sized> UserStore for std::sync::Arc<S> {
    async fn insert_user(&self, user: User, token_hash: String) -> Result<(), StoreError> {
        (**self).insert_user(user, token_hash).await
    }
    async fn user_by_token_hash(&self, token_hash: &str) -> Result<Option<User>, StoreError> {
        (**self).user_by_token_hash(token_hash).await
    }
    async fn get_user(&self, id: &str) -> Result<Option<User>, StoreError> {
        (**self).get_user(id).await
    }
    async fn put_arbiter_key(&self, user_id: &str, public_key: &str, at: i64) -> Result<(), StoreError> {
        (**self).put_arbiter_key(user_id, public_key, at).await
    }
    async fn arbiter_key(&self, user_id: &str) -> Result<Option<String>, StoreError> {
        (**self).arbiter_key(user_id).await
    }
    async fn put_node_connection(&self, conn: NodeConnection) -> Result<(), StoreError> {
        (**self).put_node_connection(conn).await
    }
    async fn node_connection(&self, user_id: &str) -> Result<Option<NodeConnection>, StoreError> {
        (**self).node_connection(user_id).await
    }
    async fn delete_node_connection(&self, user_id: &str) -> Result<bool, StoreError> {
        (**self).delete_node_connection(user_id).await
    }
    async fn insert_api_key(&self, key: ApiKey, key_hash: String) -> Result<(), StoreError> {
        (**self).insert_api_key(key, key_hash).await
    }
    async fn api_key_by_hash(&self, key_hash: &str) -> Result<Option<ApiKey>, StoreError> {
        (**self).api_key_by_hash(key_hash).await
    }
    async fn list_api_keys(&self, user_id: &str) -> Result<Vec<ApiKey>, StoreError> {
        (**self).list_api_keys(user_id).await
    }
    async fn revoke_api_key(&self, user_id: &str, id: &str, at: i64) -> Result<bool, StoreError> {
        (**self).revoke_api_key(user_id, id, at).await
    }
    async fn touch_api_key(&self, id: &str, at: i64) -> Result<(), StoreError> {
        (**self).touch_api_key(id, at).await
    }
    async fn insert_webhook(&self, hook: Webhook) -> Result<(), StoreError> {
        (**self).insert_webhook(hook).await
    }
    async fn get_webhook(&self, id: &str) -> Result<Option<Webhook>, StoreError> {
        (**self).get_webhook(id).await
    }
    async fn list_webhooks(&self, user_id: &str) -> Result<Vec<Webhook>, StoreError> {
        (**self).list_webhooks(user_id).await
    }
    async fn delete_webhook(&self, user_id: &str, id: &str) -> Result<bool, StoreError> {
        (**self).delete_webhook(user_id, id).await
    }
    async fn due_webhooks(&self, now: i64, limit: u32) -> Result<Vec<Webhook>, StoreError> {
        (**self).due_webhooks(now, limit).await
    }
    async fn record_webhook_progress(&self, id: &str, progress: WebhookProgress) -> Result<(), StoreError> {
        (**self).record_webhook_progress(id, progress).await
    }
}

#[async_trait]
impl PayoutStore for MemoryStore {
    async fn ledger_totals(&self, user_id: &str, limits: &WithdrawalLimits) -> Result<LedgerTotals, StoreError> {
        Ok(self.inner.lock().unwrap().ledger_totals(user_id, limits))
    }

    async fn insert_withdrawal(&self, w: Withdrawal, limits: &WithdrawalLimits) -> Result<bool, StoreError> {
        let mut g = self.inner.lock().unwrap();
        let taken = g.withdrawals.iter().any(|x| x.id == w.id || x.payment_hash == w.payment_hash);
        if taken {
            return Err(StoreError::Conflict);
        }
        let t = g.ledger_totals(&w.user_id, limits);
        if t.available() < w.debit() || t.withdrawn_in_window_sats + w.amount_sats > limits.window_cap_sats {
            return Ok(false);
        }
        g.withdrawals.push(w);
        Ok(true)
    }

    async fn get_withdrawal(&self, id: &str) -> Result<Option<Withdrawal>, StoreError> {
        Ok(self.inner.lock().unwrap().withdrawals.iter().find(|w| w.id == id).cloned())
    }

    async fn list_withdrawals(&self, user_id: &str) -> Result<Vec<Withdrawal>, StoreError> {
        let mut list: Vec<Withdrawal> =
            self.inner.lock().unwrap().withdrawals.iter().filter(|w| w.user_id == user_id).cloned().collect();
        list.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.id.cmp(&b.id)));
        list.truncate(100);
        Ok(list)
    }

    async fn pending_withdrawals(&self) -> Result<Vec<Withdrawal>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .withdrawals
            .iter()
            .filter(|w| w.state == WithdrawalState::Pending)
            .cloned()
            .collect())
    }

    async fn finish_withdrawal(
        &self,
        id: &str,
        state: WithdrawalState,
        fee_sats: Option<u64>,
        failure: Option<String>,
        at: i64,
    ) -> Result<bool, StoreError> {
        let mut g = self.inner.lock().unwrap();
        match g.withdrawals.iter_mut().find(|w| w.id == id && w.state == WithdrawalState::Pending) {
            Some(w) => {
                w.state = state;
                w.fee_sats = fee_sats;
                w.failure = failure;
                w.updated_at = at;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn insert_ramp_order(&self, order: RampOrder) -> Result<(), StoreError> {
        let mut g = self.inner.lock().unwrap();
        let taken = g.ramp_orders.iter().any(|o| {
            o.id == order.id
                || (order.escrow_id.is_some() && o.escrow_id == order.escrow_id && !o.state.is_terminal())
        });
        if taken {
            return Err(StoreError::Conflict);
        }
        g.ramp_orders.push(order);
        Ok(())
    }

    async fn get_ramp_order(&self, id: &str) -> Result<Option<RampOrder>, StoreError> {
        Ok(self.inner.lock().unwrap().ramp_orders.iter().find(|o| o.id == id).cloned())
    }

    async fn list_ramp_orders(&self, user_id: &str) -> Result<Vec<RampOrder>, StoreError> {
        let mut list: Vec<RampOrder> =
            self.inner.lock().unwrap().ramp_orders.iter().filter(|o| o.user_id == user_id).cloned().collect();
        list.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.id.cmp(&b.id)));
        list.truncate(100);
        Ok(list)
    }

    async fn open_ramp_orders(&self) -> Result<Vec<RampOrder>, StoreError> {
        Ok(self.inner.lock().unwrap().ramp_orders.iter().filter(|o| !o.state.is_terminal()).cloned().collect())
    }

    async fn ramp_order_for_escrow(&self, escrow_id: &str) -> Result<Option<RampOrder>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .ramp_orders
            .iter()
            .filter(|o| o.escrow_id.as_deref() == Some(escrow_id))
            .max_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| b.id.cmp(&a.id)))
            .cloned())
    }

    async fn update_ramp_order(
        &self,
        id: &str,
        expected: RampState,
        to: RampState,
        update: RampUpdate,
        at: i64,
    ) -> Result<bool, StoreError> {
        let mut g = self.inner.lock().unwrap();
        match g.ramp_orders.iter_mut().find(|o| o.id == id && o.state == expected) {
            Some(o) => {
                o.state = to;
                o.updated_at = at;
                if update.note.is_some() {
                    o.note = update.note;
                }
                if update.fiat_reference.is_some() {
                    o.fiat_reference = update.fiat_reference;
                }
                if let Some(bank) = update.refund_to {
                    o.bank = bank;
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn get_withdrawal_pin(&self, user_id: &str) -> Result<Option<WithdrawalPin>, StoreError> {
        Ok(self.inner.lock().unwrap().pins.iter().find(|(u, _)| u == user_id).map(|(_, p)| p.clone()))
    }

    async fn set_withdrawal_pin(&self, user_id: &str, pin_hash: &str, at: i64) -> Result<(), StoreError> {
        let mut g = self.inner.lock().unwrap();
        g.pins.retain(|(u, _)| u != user_id);
        let pin = WithdrawalPin { pin_hash: pin_hash.to_string(), failures: 0, locked_until: None, updated_at: at };
        g.pins.push((user_id.to_string(), pin));
        Ok(())
    }

    async fn withdrawal_pin_failed(&self, user_id: &str, max_failures: u32, lock_until: i64) -> Result<(), StoreError> {
        let mut g = self.inner.lock().unwrap();
        if let Some((_, p)) = g.pins.iter_mut().find(|(u, _)| u == user_id) {
            p.failures += 1;
            if p.failures >= max_failures {
                p.locked_until = Some(lock_until);
            }
        }
        Ok(())
    }

    async fn withdrawal_pin_ok(&self, user_id: &str) -> Result<(), StoreError> {
        let mut g = self.inner.lock().unwrap();
        if let Some((_, p)) = g.pins.iter_mut().find(|(u, _)| u == user_id) {
            p.failures = 0;
            p.locked_until = None;
        }
        Ok(())
    }
}

#[async_trait]
impl<S: PayoutStore + ?Sized> PayoutStore for std::sync::Arc<S> {
    async fn ledger_totals(&self, user_id: &str, limits: &WithdrawalLimits) -> Result<LedgerTotals, StoreError> {
        (**self).ledger_totals(user_id, limits).await
    }
    async fn insert_withdrawal(&self, w: Withdrawal, limits: &WithdrawalLimits) -> Result<bool, StoreError> {
        (**self).insert_withdrawal(w, limits).await
    }
    async fn get_withdrawal(&self, id: &str) -> Result<Option<Withdrawal>, StoreError> {
        (**self).get_withdrawal(id).await
    }
    async fn list_withdrawals(&self, user_id: &str) -> Result<Vec<Withdrawal>, StoreError> {
        (**self).list_withdrawals(user_id).await
    }
    async fn pending_withdrawals(&self) -> Result<Vec<Withdrawal>, StoreError> {
        (**self).pending_withdrawals().await
    }
    async fn finish_withdrawal(
        &self,
        id: &str,
        state: WithdrawalState,
        fee_sats: Option<u64>,
        failure: Option<String>,
        at: i64,
    ) -> Result<bool, StoreError> {
        (**self).finish_withdrawal(id, state, fee_sats, failure, at).await
    }
    async fn insert_ramp_order(&self, order: RampOrder) -> Result<(), StoreError> {
        (**self).insert_ramp_order(order).await
    }
    async fn get_ramp_order(&self, id: &str) -> Result<Option<RampOrder>, StoreError> {
        (**self).get_ramp_order(id).await
    }
    async fn list_ramp_orders(&self, user_id: &str) -> Result<Vec<RampOrder>, StoreError> {
        (**self).list_ramp_orders(user_id).await
    }
    async fn open_ramp_orders(&self) -> Result<Vec<RampOrder>, StoreError> {
        (**self).open_ramp_orders().await
    }
    async fn ramp_order_for_escrow(&self, escrow_id: &str) -> Result<Option<RampOrder>, StoreError> {
        (**self).ramp_order_for_escrow(escrow_id).await
    }
    async fn update_ramp_order(
        &self,
        id: &str,
        expected: RampState,
        to: RampState,
        update: RampUpdate,
        at: i64,
    ) -> Result<bool, StoreError> {
        (**self).update_ramp_order(id, expected, to, update, at).await
    }
    async fn get_withdrawal_pin(&self, user_id: &str) -> Result<Option<WithdrawalPin>, StoreError> {
        (**self).get_withdrawal_pin(user_id).await
    }
    async fn set_withdrawal_pin(&self, user_id: &str, pin_hash: &str, at: i64) -> Result<(), StoreError> {
        (**self).set_withdrawal_pin(user_id, pin_hash, at).await
    }
    async fn withdrawal_pin_failed(&self, user_id: &str, max_failures: u32, lock_until: i64) -> Result<(), StoreError> {
        (**self).withdrawal_pin_failed(user_id, max_failures, lock_until).await
    }
    async fn withdrawal_pin_ok(&self, user_id: &str) -> Result<(), StoreError> {
        (**self).withdrawal_pin_ok(user_id).await
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn memory_store_meets_the_payout_store_contract() {
        crate::testing::payout_store_contract(&super::MemoryStore::new()).await;
    }
}
