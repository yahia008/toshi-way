//! The escrow service. Payment state comes only from the node: every move to LOCKED, SETTLED,
//! REFUNDED or EXPIRED_UNPAID is driven by a node event or a node lookup, never by a caller's claim.

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    adapter::{AdapterError, HoldInvoiceRequest, LightningAdapter, NodeDirectory},
    model::{
        AuditEntry, Escrow, InvoiceEvent, NodeEventRecord, NodeInvoiceState, NodeKind, PaymentHash, Preimage,
    },
    state::EscrowState,
    store::{EscrowUpdate, Store, StoreError, UserTotals},
    timing::{TimingConfig, TimingError},
};

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("escrow not found")]
    NotFound,
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error(transparent)]
    Timing(#[from] TimingError),
    #[error("cannot {action} an escrow in state {state}")]
    InvalidState { action: &'static str, state: EscrowState },
    #[error("preimage does not match the payment hash")]
    WrongPreimage,
    #[error("the settle-by deadline has passed")]
    TooLate,
    #[error("payment hash is already used by another escrow")]
    HashInUse,
    #[error("node error: {0}")]
    Adapter(#[from] AdapterError),
    #[error("store error: {0}")]
    Store(StoreError),
}

/// How many delivery and evidence notes one person may add to one escrow.
pub const MAX_NOTES_PER_PARTY: usize = 10;

/// Base64 text of a plausible size for a P-256 ECIES blob (65-byte key + 12-byte nonce + 48 bytes).
fn valid_sealed(s: &str) -> bool {
    (100..=1024).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

impl From<StoreError> for ServiceError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound => ServiceError::NotFound,
            other => ServiceError::Store(other),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewEscrow {
    /// `None` creates an open offer: the first user to accept becomes the buyer.
    pub buyer_id: Option<String>,
    pub seller_id: String,
    /// The arbiter and the public key recorded for them now. Both or neither.
    pub arbiter: Option<(String, String)>,
    pub amount_sats: u64,
    /// A display-only fiat value, already worked out by the caller.
    pub fiat: Option<crate::fiat::FiatQuote>,
    /// The seller's own order number, if any. See [`Escrow::reference`].
    pub reference: Option<String>,
    /// A link back for the buyer, if any. See [`Escrow::return_url`].
    pub return_url: Option<String>,
    pub description: String,
    pub deadline_at: i64,
}

/// What an arbiter decides about a disputed escrow.
pub enum Decision {
    /// Pay the seller: needs the release secret, which the arbiter decrypted.
    Release(Preimage),
    /// Give the buyer their money back.
    Refund,
}

/// An invoice to watch and where to find it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchTarget {
    pub payment_hash: PaymentHash,
    pub seller_id: String,
    pub node: NodeKind,
}

#[derive(Debug, Default)]
pub struct TickReport {
    pub cancelled: Vec<String>,
    pub errors: Vec<(String, String)>,
}

#[derive(Debug, Default)]
pub struct ReconcileReport {
    pub checked: usize,
    pub changed: Vec<String>,
    pub errors: Vec<(String, String)>,
}

pub struct EscrowService<A, S> {
    adapter: A,
    store: S,
    timing: TimingConfig,
}

/// What a node's invoice state means for an escrow currently in `state`.
/// `Ok(None)` = nothing to do; `Err(())` = the combination should be impossible.
fn target_state(state: EscrowState, node: NodeInvoiceState) -> Result<Option<EscrowState>, ()> {
    use EscrowState as E;
    use NodeInvoiceState as N;
    match (state, node) {
        (E::InvoiceIssued, N::Accepted) => Ok(Some(E::Locked)),
        (E::InvoiceIssued, N::Canceled) => Ok(Some(E::ExpiredUnpaid)),
        (E::Locked | E::Disputed, N::Settled) => Ok(Some(E::Settled)),
        (E::Locked | E::Disputed, N::Canceled) => Ok(Some(E::Refunded)),
        (E::InvoiceIssued, N::Open) | (E::Locked | E::Disputed, N::Accepted) => Ok(None),
        (E::InvoiceIssued, N::Settled) => Err(()),
        (E::Locked | E::Disputed, N::Open) => Err(()),
        // Created has no invoice; terminal states never move again.
        _ => Ok(None),
    }
}

impl<A: NodeDirectory, S: Store> EscrowService<A, S> {
    pub fn new(adapter: A, store: S, timing: TimingConfig) -> Self {
        Self { adapter, store, timing }
    }

    pub async fn get(&self, id: &str) -> Result<Escrow, ServiceError> {
        self.store.get(id).await?.ok_or(ServiceError::NotFound)
    }

    pub async fn list_active(&self) -> Result<Vec<Escrow>, ServiceError> {
        Ok(self.store.list_active().await?)
    }

    pub async fn list_for_user(&self, user_id: &str) -> Result<Vec<Escrow>, ServiceError> {
        Ok(self.store.list_for_user(user_id).await?)
    }

    pub async fn totals_for_user(&self, user_id: &str) -> Result<UserTotals, ServiceError> {
        Ok(self.store.totals_for_user(user_id).await?)
    }

    pub async fn node_events(&self, id: &str) -> Result<Vec<NodeEventRecord>, ServiceError> {
        Ok(self.store.node_events(id).await?)
    }

    pub async fn node_health(&self) -> Result<(), ServiceError> {
        Ok(self.adapter.ping().await?)
    }

    /// The node directory, for code that manages sellers' node connections.
    pub fn directory(&self) -> &A {
        &self.adapter
    }

    /// The node an existing escrow lives on.
    async fn node_of(&self, e: &Escrow) -> Result<A::Node, AdapterError> {
        self.adapter.node(&e.seller_id, e.node).await
    }

    /// Open invoices the node may still change, each with the node to watch it on.
    pub async fn watch_targets(&self) -> Result<Vec<WatchTarget>, ServiceError> {
        Ok(self
            .store
            .list_active()
            .await?
            .into_iter()
            .filter_map(|e| {
                Some(WatchTarget { payment_hash: e.payment_hash?, seller_id: e.seller_id, node: e.node })
            })
            .collect())
    }

    /// Whether the seller has escrows in flight on their own node. Their node settings must not
    /// change while that is true, or the invoices would be stranded on the old node.
    pub async fn has_active_own_node_escrows(&self, seller_id: &str) -> Result<bool, ServiceError> {
        Ok(self
            .store
            .list_active()
            .await?
            .iter()
            .any(|e| e.seller_id == seller_id && e.node == NodeKind::Own && e.payment_hash.is_some()))
    }

    /// The position of the newest audit entry, for starting a feed "from now".
    pub async fn audit_head(&self) -> Result<i64, ServiceError> {
        Ok(self.store.audit_head().await?)
    }

    pub async fn audit_log(&self, id: &str) -> Result<Vec<AuditEntry>, ServiceError> {
        Ok(self.store.audit_log(id).await?)
    }

    pub async fn create(&self, new: NewEscrow, now: i64) -> Result<Escrow, ServiceError> {
        if new.amount_sats == 0 {
            return Err(ServiceError::Invalid("amount must be greater than zero".into()));
        }
        if new.buyer_id.as_deref() == Some(new.seller_id.as_str()) {
            return Err(ServiceError::Invalid("buyer and seller must differ".into()));
        }
        self.timing.validate_deadline(now, new.deadline_at)?;
        if let Some((arbiter, _)) = &new.arbiter {
            if *arbiter == new.seller_id || new.buyer_id.as_deref() == Some(arbiter.as_str()) {
                return Err(ServiceError::Invalid("the arbiter must be someone other than the buyer and seller".into()));
            }
        }
        // Fails with `NoNode` if the seller has no node and there is no shared one.
        let node = self.adapter.kind_for_new(&new.seller_id).await?;
        let escrow = Escrow {
            id: uuid::Uuid::new_v4().to_string(),
            buyer_id: new.buyer_id,
            seller_id: new.seller_id,
            arbiter_id: new.arbiter.as_ref().map(|(id, _)| id.clone()),
            arbiter_key: new.arbiter.map(|(_, key)| key),
            arbiter_sealed: None,
            arbiter_confirmed_at: None,
            node,
            amount_sats: new.amount_sats,
            fiat: new.fiat,
            reference: new.reference,
            return_url: new.return_url,
            description: new.description,
            payment_hash: None,
            invoice: None,
            invoice_expires_at: None,
            state: EscrowState::Created,
            deadline_at: new.deadline_at,
            created_at: now,
            updated_at: now,
        };
        self.store.insert(escrow.clone()).await?;
        Ok(escrow)
    }

    /// The buyer accepts and supplies `H`; the seller's node issues the hold invoice.
    /// Repeating the call with the same hash returns the same escrow. On an open offer (no buyer
    /// yet) the accepting user becomes the buyer; if two race, the state compare-and-set picks one.
    pub async fn accept(
        &self,
        id: &str,
        hash: PaymentHash,
        actor: &str,
        now: i64,
    ) -> Result<Escrow, ServiceError> {
        self.accept_with(id, hash, actor, now, None).await
    }

    /// `accept`, plus the buyer's release secret encrypted to the arbiter. An escrow with an
    /// arbiter cannot be accepted without it (otherwise arbitration would have nothing to release),
    /// and one without cannot take it. The server never sees what is inside.
    pub async fn accept_with(
        &self,
        id: &str,
        hash: PaymentHash,
        actor: &str,
        now: i64,
        sealed: Option<String>,
    ) -> Result<Escrow, ServiceError> {
        let e = self.get(id).await?;
        match e.buyer_id.as_deref() {
            Some(buyer) if buyer != actor => {
                return Err(ServiceError::Invalid("only the buyer can accept".into()));
            }
            None if actor == e.seller_id => {
                return Err(ServiceError::Invalid("the seller cannot be the buyer".into()));
            }
            _ => {}
        }
        match e.state {
            EscrowState::InvoiceIssued if e.payment_hash == Some(hash) => return Ok(e),
            EscrowState::Created => {}
            state => return Err(ServiceError::InvalidState { action: "accept", state }),
        }
        if let Some(other) = self.store.get_by_hash(&hash).await? {
            if other.id != e.id {
                return Err(ServiceError::HashInUse);
            }
        }
        match (&e.arbiter_id, &sealed) {
            (Some(_), None) => {
                return Err(ServiceError::Invalid("this escrow has an arbiter: the sealed release secret is required".into()))
            }
            (None, Some(_)) => {
                return Err(ServiceError::Invalid("this escrow has no arbiter, so there is nothing to seal the secret to".into()))
            }
            (Some(_), Some(s)) if !valid_sealed(s) => {
                return Err(ServiceError::Invalid("the sealed secret is not valid".into()))
            }
            _ => {}
        }
        self.timing.validate_deadline(now, e.deadline_at)?;

        let expiry_secs = self.timing.invoice_expiry_secs.min(e.deadline_at - now);
        let invoice = self
            .node_of(&e)
            .await?
            .create_hold_invoice(HoldInvoiceRequest {
                payment_hash: hash,
                amount_sats: e.amount_sats,
                memo: format!("escrow {}", e.id),
                expiry_secs,
                final_cltv_delta: self.timing.final_cltv_delta(now, e.deadline_at),
            })
            .await?;
        let update = EscrowUpdate {
            arbiter_sealed: sealed,
            buyer_id: e.buyer_id.is_none().then(|| actor.to_string()),
            payment_hash: Some(hash),
            invoice: Some(invoice),
            invoice_expires_at: Some(now + expiry_secs),
            ..Default::default()
        };
        self.move_to(&e, EscrowState::InvoiceIssued, update, actor, "accept", "hold invoice issued", now)
            .await
    }

    /// Apply an event from the node's invoice subscription. Unknown invoices are ignored.
    pub async fn apply_event(&self, ev: InvoiceEvent, now: i64) -> Result<Option<Escrow>, ServiceError> {
        let Some(e) = self.store.get_by_hash(&ev.payment_hash).await? else {
            return Ok(None);
        };
        let raw = format!("{}:{}", hex::encode(ev.payment_hash), ev.state.as_str());
        self.store
            .record_node_event(NodeEventRecord {
                escrow_id: e.id.clone(),
                node_invoice_state: ev.state,
                raw_event_hash: hex::encode(Sha256::digest(raw)),
                received_at: now,
            })
            .await?;
        Ok(Some(self.apply_node_state(e, ev.state, "node", "node invoice event", now).await?))
    }

    /// Submit the preimage. Verified against `H` before the node is touched; state changes only
    /// when the node reports the invoice settled.
    pub async fn release(
        &self,
        id: &str,
        preimage: &Preimage,
        actor: &str,
        now: i64,
    ) -> Result<Escrow, ServiceError> {
        self.release_as(id, preimage, actor, "release", now).await
    }

    async fn release_as(
        &self,
        id: &str,
        preimage: &Preimage,
        actor: &str,
        reason: &str,
        now: i64,
    ) -> Result<Escrow, ServiceError> {
        let e = self.get(id).await?;
        let hash = e.payment_hash.ok_or(ServiceError::InvalidState { action: "release", state: e.state })?;
        if preimage.hash() != hash {
            return Err(ServiceError::WrongPreimage);
        }
        match e.state {
            EscrowState::Settled => return Ok(e),
            EscrowState::Locked | EscrowState::Disputed => {}
            state => return Err(ServiceError::InvalidState { action: "release", state }),
        }
        if now >= e.deadline_at {
            return Err(ServiceError::TooLate);
        }
        self.node_of(&e).await?.settle_invoice(preimage).await?;
        self.sync_from_node(e, actor, reason, now).await
    }

    /// Cancel the hold invoice. Repeating it on an already cancelled escrow is a no-op.
    /// An offer nobody has accepted has no invoice, so withdrawing it needs no node call.
    pub async fn cancel(&self, id: &str, actor: &str, reason: &str, now: i64) -> Result<Escrow, ServiceError> {
        let e = self.get(id).await?;
        let hash = match (e.state, e.payment_hash) {
            (EscrowState::Refunded | EscrowState::ExpiredUnpaid, _) => return Ok(e),
            (EscrowState::Created, _) => {
                return match self
                    .move_to(&e, EscrowState::ExpiredUnpaid, EscrowUpdate::default(), actor, "cancel", reason, now)
                    .await
                {
                    // A concurrent actor (the buyer accepting, the scheduler) got there first.
                    Err(ServiceError::Store(StoreError::Conflict)) => Err(ServiceError::InvalidState {
                        action: "cancel",
                        state: self.get(id).await?.state,
                    }),
                    other => other,
                };
            }
            (EscrowState::InvoiceIssued | EscrowState::Locked | EscrowState::Disputed, Some(h)) => h,
            (state, _) => return Err(ServiceError::InvalidState { action: "cancel", state }),
        };
        self.node_of(&e).await?.cancel_invoice(&hash).await?;
        self.sync_from_node(e, actor, reason, now).await
    }

    /// The arbiter says they can decrypt the buyer's sealed secret and that it matches the payment
    /// hash, so the seller can rely on arbitration. The service cannot check this itself (it cannot
    /// read the secret); it records the arbiter's word. Repeating it changes nothing.
    pub async fn confirm_arbiter(&self, id: &str, actor: &str, now: i64) -> Result<Escrow, ServiceError> {
        let e = self.get(id).await?;
        if e.arbiter_id.as_deref() != Some(actor) {
            return Err(ServiceError::Invalid("only the arbiter can confirm".into()));
        }
        if e.arbiter_confirmed_at.is_some() {
            return Ok(e);
        }
        if e.arbiter_sealed.is_none() || !matches!(e.state, EscrowState::InvoiceIssued | EscrowState::Locked) {
            return Err(ServiceError::InvalidState { action: "confirm", state: e.state });
        }
        let audit = AuditEntry {
            escrow_id: e.id.clone(),
            actor: actor.to_string(),
            action: "arbiter_confirm".into(),
            from_state: e.state,
            to_state: e.state,
            reason: "arbiter can release this escrow".into(),
            created_at: now,
        };
        let update = EscrowUpdate { arbiter_confirmed_at: Some(now), ..Default::default() };
        match self.store.transition(&e.id, e.state, update, audit).await {
            Err(StoreError::Conflict) => Ok(self.get(id).await?),
            other => Ok(other?),
        }
    }

    /// The arbiter settles a disputed escrow. Only the named arbiter, only while it is disputed
    /// and before the deadline passes (the hold cannot be extended: after it, the scheduler
    /// refunds). Releasing needs the secret and is checked against the payment hash like any other
    /// release; refunding cancels the hold. Repeating the same decision is a no-op.
    pub async fn arbitrate(
        &self,
        id: &str,
        arbiter: &str,
        decision: Decision,
        note: &str,
        now: i64,
    ) -> Result<Escrow, ServiceError> {
        let e = self.get(id).await?;
        if e.arbiter_id.as_deref() != Some(arbiter) {
            return Err(ServiceError::Invalid("only the arbiter can decide".into()));
        }
        let reason = |verb: &str| match note.trim() {
            "" => format!("arbiter decided: {verb}"),
            n => format!("arbiter decided: {verb} ({n})"),
        };
        match (&decision, e.state) {
            (Decision::Release(_), EscrowState::Settled) | (Decision::Refund, EscrowState::Refunded) => return Ok(e),
            (_, EscrowState::Disputed) => {}
            (_, state) => return Err(ServiceError::InvalidState { action: "arbitrate", state }),
        }
        if now >= e.deadline_at {
            return Err(ServiceError::TooLate);
        }
        // Put the decision on the record before touching the node. The node reports the settlement
        // or cancellation itself and can win the race to record the state change, which would
        // credit "the node" and drop the arbiter's name and note.
        let verb = match &decision {
            Decision::Release(p) => {
                // A decision that cannot be carried out is not a decision: check before recording.
                if Some(p.hash()) != e.payment_hash {
                    return Err(ServiceError::WrongPreimage);
                }
                "release to the seller"
            }
            Decision::Refund => "refund the buyer",
        };
        self.record_decision(&e, arbiter, &reason(verb), now).await?;
        match decision {
            Decision::Release(p) => self.release_as(id, &p, arbiter, &reason(verb), now).await,
            Decision::Refund => self.cancel(id, arbiter, &reason(verb), now).await,
        }
    }

    /// Add the arbiter's decision to the audit log without changing the state. Repeating the same
    /// decision (for example after a failed node call) does not add a second entry.
    async fn record_decision(&self, e: &Escrow, arbiter: &str, reason: &str, now: i64) -> Result<(), ServiceError> {
        let log = self.store.audit_log(&e.id).await?;
        if log.iter().rev().find(|a| a.action == "arbiter_decision").is_some_and(|a| a.reason == reason) {
            return Ok(());
        }
        let audit = AuditEntry {
            escrow_id: e.id.clone(),
            actor: arbiter.to_string(),
            action: "arbiter_decision".into(),
            from_state: e.state,
            to_state: e.state,
            reason: reason.to_string(),
            created_at: now,
        };
        match self.store.transition(&e.id, e.state, EscrowUpdate::default(), audit).await {
            // The escrow moved on in the meantime (a node event); the action below will see it.
            Ok(_) | Err(StoreError::Conflict) => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Flag a dispute. Does not extend the hold: the deadline still applies.
    pub async fn dispute(&self, id: &str, actor: &str, reason: &str, now: i64) -> Result<Escrow, ServiceError> {
        let e = self.get(id).await?;
        match e.state {
            EscrowState::Disputed => return Ok(e),
            EscrowState::Locked => {}
            state => return Err(ServiceError::InvalidState { action: "dispute", state }),
        }
        if now >= e.deadline_at {
            return Err(ServiceError::TooLate);
        }
        self.move_to(&e, EscrowState::Disputed, EscrowUpdate::default(), actor, "dispute", reason, now)
            .await
    }

    /// A note on a locked or disputed escrow: the seller saying the goods or work were delivered, or
    /// either side adding evidence for whoever has to decide. It changes no state and moves no money;
    /// it is written to the audit log so the history shows who said what, and when. Like a dispute it
    /// does not extend the hold.
    pub async fn add_note(&self, id: &str, actor: &str, kind: &str, text: &str, now: i64) -> Result<Escrow, ServiceError> {
        // The state can move between reading and writing (a dispute opens, say); try again once.
        for _ in 0..2 {
            let e = self.get(id).await?;
            if !matches!(e.state, EscrowState::Locked | EscrowState::Disputed) {
                return Err(ServiceError::InvalidState { action: "add a note to", state: e.state });
            }
            if now >= e.deadline_at {
                return Err(ServiceError::TooLate);
            }
            let mine = self
                .store
                .audit_log(id)
                .await?
                .iter()
                .filter(|a| a.actor == actor && matches!(a.action.as_str(), "delivered" | "evidence"))
                .count();
            if mine >= MAX_NOTES_PER_PARTY {
                return Err(ServiceError::Invalid(format!("at most {MAX_NOTES_PER_PARTY} notes each")));
            }
            let audit = AuditEntry {
                escrow_id: e.id.clone(),
                actor: actor.to_string(),
                action: kind.to_string(),
                from_state: e.state,
                to_state: e.state,
                reason: text.to_string(),
                created_at: now,
            };
            match self.store.transition(&e.id, e.state, EscrowUpdate::default(), audit).await {
                Err(StoreError::Conflict) => continue,
                other => return Ok(other?),
            }
        }
        Err(ServiceError::Invalid("the escrow changed while the note was being saved; try again".into()))
    }

    /// The scheduler pass: cancel holds at their deadline, unpaid invoices once they expire, and
    /// offers nobody accepted before their deadline.
    pub async fn tick(&self, now: i64) -> Result<TickReport, ServiceError> {
        let mut report = TickReport::default();
        for e in self.store.list_active().await? {
            let due = match e.state {
                EscrowState::Created => now >= e.deadline_at,
                EscrowState::Locked | EscrowState::Disputed => now >= e.deadline_at,
                EscrowState::InvoiceIssued => e.invoice_expires_at.is_some_and(|t| now >= t),
                _ => false,
            };
            if !due {
                continue;
            }
            match self.cancel(&e.id, "scheduler", "deadline reached", now).await {
                Ok(_) => report.cancelled.push(e.id),
                Err(err) => report.errors.push((e.id, err.to_string())),
            }
        }
        Ok(report)
    }

    /// Startup recovery: compare every open escrow with the node and adopt the node's view.
    pub async fn reconcile(&self, now: i64) -> Result<ReconcileReport, ServiceError> {
        let mut report = ReconcileReport::default();
        for e in self.store.list_active().await? {
            let Some(hash) = e.payment_hash else { continue };
            report.checked += 1;
            let before = e.state;
            let result = match self.lookup(&e, &hash).await {
                Ok(Some(inv)) => self.apply_node_state(e.clone(), inv.state, "reconcile", "startup reconciliation", now).await,
                Ok(None) => {
                    self.move_to(&e, EscrowState::Failed, EscrowUpdate::default(), "reconcile", "reconcile", "node has no such invoice", now)
                        .await
                }
                Err(err) => Err(err.into()),
            };
            match result {
                Ok(after) if after.state != before => report.changed.push(after.id),
                Ok(_) => {}
                Err(err) => report.errors.push((e.id, err.to_string())),
            }
        }
        Ok(report)
    }

    async fn lookup(&self, e: &Escrow, hash: &PaymentHash) -> Result<Option<crate::model::NodeInvoice>, AdapterError> {
        self.node_of(e).await?.lookup_invoice(hash).await
    }

    async fn sync_from_node(
        &self,
        e: Escrow,
        actor: &str,
        reason: &str,
        now: i64,
    ) -> Result<Escrow, ServiceError> {
        let hash = e.payment_hash.expect("escrows with an invoice have a hash");
        match self.lookup(&e, &hash).await? {
            Some(inv) => self.apply_node_state(e, inv.state, actor, reason, now).await,
            None => Ok(e),
        }
    }

    /// Move the escrow to whatever the node's invoice state implies, retrying if a concurrent
    /// actor got there first.
    async fn apply_node_state(
        &self,
        mut e: Escrow,
        node: NodeInvoiceState,
        actor: &str,
        reason: &str,
        now: i64,
    ) -> Result<Escrow, ServiceError> {
        for _ in 0..3 {
            let (to, why) = match target_state(e.state, node) {
                Ok(None) => return Ok(e),
                Ok(Some(to)) => (to, reason.to_string()),
                Err(()) => (EscrowState::Failed, format!("node reports {node:?} while escrow is {}", e.state)),
            };
            match self.move_to(&e, to, EscrowUpdate::default(), actor, "node_state", &why, now).await {
                Err(ServiceError::Store(StoreError::Conflict)) => e = self.get(&e.id).await?,
                other => return other,
            }
        }
        Err(ServiceError::Store(StoreError::Conflict))
    }

    #[allow(clippy::too_many_arguments)]
    async fn move_to(
        &self,
        e: &Escrow,
        to: EscrowState,
        update: EscrowUpdate,
        actor: &str,
        action: &str,
        reason: &str,
        now: i64,
    ) -> Result<Escrow, ServiceError> {
        if !e.state.can_transition_to(to) {
            return Err(ServiceError::InvalidState { action: "transition", state: e.state });
        }
        let audit = AuditEntry {
            escrow_id: e.id.clone(),
            actor: actor.to_string(),
            action: action.to_string(),
            from_state: e.state,
            to_state: to,
            reason: reason.to_string(),
            created_at: now,
        };
        Ok(self.store.transition(&e.id, e.state, update, audit).await?)
    }
}
