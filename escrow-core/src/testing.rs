//! Behaviour every [`Store`] implementation must provide. Each backend's tests call
//! [`store_contract`] with a fresh, empty store so MemoryStore, SQLite and Postgres stay interchangeable.

use crate::{
    model::{ApiKey, AuditEntry, Escrow, NodeConnection, NodeEventRecord, NodeInvoiceState, NodeKind, Role, Scope, User, Webhook, WebhookProgress},
    state::EscrowState,
    store::{EscrowUpdate, Store, StoreError, UserStore},
};

fn escrow(id: &str) -> Escrow {
    Escrow {
        id: id.into(),
        buyer_id: Some("buyer".into()),
        arbiter_id: None,
        arbiter_key: None,
        arbiter_sealed: None,
        arbiter_confirmed_at: None,
        node: NodeKind::Shared,
        fiat: None,
        reference: None,
        return_url: None,
        seller_id: "seller".into(),
        amount_sats: 1000,
        description: "parcel".into(),
        payment_hash: None,
        invoice: None,
        invoice_expires_at: None,
        state: EscrowState::Created,
        deadline_at: 5000,
        created_at: 100,
        updated_at: 100,
    }
}

fn audit(id: &str, from: EscrowState, to: EscrowState, at: i64) -> AuditEntry {
    AuditEntry {
        escrow_id: id.into(),
        actor: "tester".into(),
        action: "act".into(),
        from_state: from,
        to_state: to,
        reason: "because".into(),
        created_at: at,
    }
}

pub async fn store_contract(store: &dyn Store) {
    use EscrowState::*;

    // Round trip and uniqueness.
    let e = escrow("a");
    store.insert(e.clone()).await.unwrap();
    assert_eq!(store.get("a").await.unwrap(), Some(e.clone()));

    // A business's order number and link back survive storage (and are absent when not set).
    let mut shop = escrow("shop");
    shop.buyer_id = Some("shop-buyer".into());
    shop.seller_id = "shop-seller".into();
    shop.reference = Some("order #1001".into());
    shop.return_url = Some("https://shop.example.com/thanks?o=1001".into());
    store.insert(shop.clone()).await.unwrap();
    assert_eq!(store.get("shop").await.unwrap(), Some(shop));
    assert_eq!(store.get("a").await.unwrap().unwrap().reference, None);
    store.transition("shop", EscrowState::Created, EscrowUpdate::default(), audit("shop", EscrowState::Created, EscrowState::Failed, 150)).await.unwrap();
    assert_eq!(store.get("missing").await.unwrap(), None);
    assert_eq!(store.insert(e.clone()).await, Err(StoreError::Conflict));

    // A transition applies its update and appends its audit entry together.
    let hash = [7u8; 32];
    let update = EscrowUpdate {
        arbiter_sealed: None,
        arbiter_confirmed_at: None,
        buyer_id: None,
        payment_hash: Some(hash),
        invoice: Some("lnbcrt1abc".into()),
        invoice_expires_at: Some(700),
    };
    let moved = store.transition("a", Created, update, audit("a", Created, InvoiceIssued, 200)).await.unwrap();
    assert_eq!(moved.state, InvoiceIssued);
    assert_eq!(moved.payment_hash, Some(hash));
    assert_eq!(moved.invoice.as_deref(), Some("lnbcrt1abc"));
    assert_eq!(moved.invoice_expires_at, Some(700));
    assert_eq!(moved.updated_at, 200);
    assert_eq!(store.get("a").await.unwrap(), Some(moved.clone()));
    assert_eq!(store.get_by_hash(&hash).await.unwrap(), Some(moved));
    assert_eq!(store.get_by_hash(&[9u8; 32]).await.unwrap(), None);

    // A transition from the wrong state fails and leaves no trace.
    let stale = store
        .transition("a", Created, EscrowUpdate::default(), audit("a", Created, Locked, 300))
        .await;
    assert_eq!(stale, Err(StoreError::Conflict));
    assert_eq!(store.get("a").await.unwrap().unwrap().state, InvoiceIssued);
    assert_eq!(store.audit_log("a").await.unwrap().len(), 1);

    // An empty update keeps the fields set earlier.
    let locked = store
        .transition("a", InvoiceIssued, EscrowUpdate::default(), audit("a", InvoiceIssued, Locked, 300))
        .await
        .unwrap();
    assert_eq!(locked.payment_hash, Some(hash));
    assert_eq!(locked.invoice.as_deref(), Some("lnbcrt1abc"));

    // Unknown escrow.
    let missing = store
        .transition("nope", Created, EscrowUpdate::default(), audit("nope", Created, InvoiceIssued, 1))
        .await;
    assert_eq!(missing, Err(StoreError::NotFound));

    // The audit log is ordered and scoped to its escrow.
    store.insert(escrow("b")).await.unwrap();
    store
        .transition("b", Created, EscrowUpdate::default(), audit("b", Created, Failed, 400))
        .await
        .unwrap();
    let log = store.audit_log("a").await.unwrap();
    assert_eq!(
        log.iter().map(|x| (x.from_state, x.to_state, x.created_at)).collect::<Vec<_>>(),
        vec![(Created, InvoiceIssued, 200), (InvoiceIssued, Locked, 300)]
    );
    assert_eq!(log[0], audit("a", Created, InvoiceIssued, 200));
    assert_eq!(store.audit_log("b").await.unwrap().len(), 1);

    // The audit log read as a feed for one seller: in order, resumable from a position, limited,
    // and never another seller's entries.
    let feed = store.audit_since("seller", 0, 100).await.unwrap();
    assert_eq!(
        feed.iter().map(|r| (r.entry.escrow_id.as_str(), r.entry.to_state)).collect::<Vec<_>>(),
        vec![("a", InvoiceIssued), ("a", Locked), ("b", Failed)]
    );
    assert!(feed.windows(2).all(|w| w[0].seq < w[1].seq), "positions only grow");
    assert_eq!(store.audit_since("seller", feed[0].seq, 100).await.unwrap(), feed[1..].to_vec(), "resume after a position");
    assert_eq!(store.audit_since("seller", 0, 2).await.unwrap(), feed[..2].to_vec(), "limited");
    assert!(store.audit_since("seller", feed[2].seq, 100).await.unwrap().is_empty());
    assert!(store.audit_since("nobody", 0, 100).await.unwrap().is_empty());
    let head = store.audit_head().await.unwrap();
    assert!(head >= feed[2].seq);
    assert!(store.audit_since("seller", head, 100).await.unwrap().is_empty(), "from the head there is nothing yet");

    // Active listing leaves out terminal escrows.
    let active: Vec<_> = store.list_active().await.unwrap().into_iter().map(|e| e.id).collect();
    assert_eq!(active, vec!["a".to_string()]);

    // Per-user listing: parties only, newest first, ties by id.
    let mut newer = escrow("c");
    newer.created_at = 200;
    newer.buyer_id = Some("someone-else".into());
    store.insert(newer).await.unwrap();
    let ids = |v: Vec<Escrow>| v.into_iter().map(|e| e.id).collect::<Vec<_>>();
    assert_eq!(ids(store.list_for_user("buyer").await.unwrap()), ["a", "b"]);
    assert_eq!(ids(store.list_for_user("seller").await.unwrap()), ["c", "a", "b"]);
    assert_eq!(ids(store.list_for_user("someone-else").await.unwrap()), ["c"]);
    assert!(store.list_for_user("nobody").await.unwrap().is_empty());

    // An arbitrated escrow keeps its arbiter, shows up in the arbiter's list, and accepts the sealed
    // secret and the arbiter's confirmation as updates.
    let mut arb = escrow("arb");
    arb.arbiter_id = Some("judge".into());
    arb.arbiter_key = Some("judge-key".into());
    arb.created_at = 45;
    store.insert(arb.clone()).await.unwrap();
    assert_eq!(store.get("arb").await.unwrap(), Some(arb));
    assert_eq!(ids(store.list_for_user("judge").await.unwrap()), ["arb"]);
    let sealed = EscrowUpdate { arbiter_sealed: Some("sealed-blob".into()), ..Default::default() };
    let e = store.transition("arb", Created, sealed, audit("arb", Created, InvoiceIssued, 46)).await.unwrap();
    assert_eq!(e.arbiter_sealed.as_deref(), Some("sealed-blob"));
    let confirm = EscrowUpdate { arbiter_confirmed_at: Some(47), ..Default::default() };
    let e = store.transition("arb", InvoiceIssued, confirm, audit("arb", InvoiceIssued, InvoiceIssued, 47)).await.unwrap();
    assert_eq!((e.state, e.arbiter_confirmed_at, e.arbiter_sealed.as_deref()), (InvoiceIssued, Some(47), Some("sealed-blob")));

    // A fiat quote is stored exactly as given, survives later transitions, and absent means absent.
    let mut priced = escrow("priced");
    priced.created_at = 40;
    priced.fiat = Some(crate::fiat::FiatQuote {
        currency: "NGN".into(),
        amount_minor: 7_312_500,
        rate_minor_per_btc: 9_750_000_000,
        quoted_at: 39,
        source: "demo rate".into(),
    });
    store.insert(priced.clone()).await.unwrap();
    assert_eq!(store.get("priced").await.unwrap(), Some(priced.clone()));
    let moved = store
        .transition("priced", Created, EscrowUpdate::default(), audit("priced", Created, InvoiceIssued, 41))
        .await
        .unwrap();
    assert_eq!(moved.fiat, priced.fiat, "the quote is locked: a transition does not touch it");
    assert_eq!(store.get("a").await.unwrap().unwrap().fiat, None);

    // An open offer has no buyer until someone accepts; the claim is applied with the transition
    // and an update without a buyer leaves it alone.
    let mut open = escrow("open");
    open.buyer_id = None;
    open.created_at = 50;
    store.insert(open).await.unwrap();
    assert_eq!(store.get("open").await.unwrap().unwrap().buyer_id, None);
    assert!(store.list_for_user("claimer").await.unwrap().is_empty());
    let claim = EscrowUpdate { buyer_id: Some("claimer".into()), ..Default::default() };
    let claimed = store.transition("open", Created, claim, audit("open", Created, InvoiceIssued, 60)).await.unwrap();
    assert_eq!(claimed.buyer_id.as_deref(), Some("claimer"));
    assert_eq!(ids(store.list_for_user("claimer").await.unwrap()), ["open"]);
    let after = store
        .transition("open", InvoiceIssued, EscrowUpdate::default(), audit("open", InvoiceIssued, Locked, 70))
        .await
        .unwrap();
    assert_eq!(after.buyer_id.as_deref(), Some("claimer"), "later updates keep the buyer");
    // The active-listing and audit assertions above ran before this, so leave the rest unchanged.

    // Node events are kept per escrow, in order.
    for (state, at) in [(NodeInvoiceState::Accepted, 310), (NodeInvoiceState::Settled, 320)] {
        store
            .record_node_event(NodeEventRecord {
                escrow_id: "a".into(),
                node_invoice_state: state,
                raw_event_hash: format!("hash-{at}"),
                received_at: at,
            })
            .await
            .unwrap();
    }
    let events = store.node_events("a").await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].node_invoice_state, NodeInvoiceState::Accepted);
    assert_eq!(events[1].raw_event_hash, "hash-320");
    assert!(store.node_events("b").await.unwrap().is_empty());
}

/// A long history: the list holds everything in play plus the newest 100 that ended, while the
/// totals still add up over all of it.
pub async fn history_contract(store: &dyn Store) {
    use EscrowState::*;
    let mut n = 0;
    let mut add = |state: EscrowState, buyer: &str, seller: &str, amount: u64| {
        n += 1;
        let mut e = escrow(&format!("h{n:03}"));
        e.state = state;
        e.buyer_id = Some(buyer.into());
        e.seller_id = seller.into();
        e.amount_sats = amount;
        e.created_at = n;
        e
    };
    let mut all = Vec::new();
    // Oldest first: two still in play, then 70 settled and 40 refunded, with "me" as the buyer.
    all.push(add(Locked, "me", "them", 500));
    all.push(add(Created, "me", "them", 400));
    for _ in 0..70 {
        all.push(add(Settled, "me", "them", 10));
    }
    for _ in 0..40 {
        all.push(add(Refunded, "me", "them", 20));
    }
    // And a few where "me" is the seller, and one where "me" only judges.
    for _ in 0..3 {
        all.push(add(Settled, "them", "me", 100));
    }
    let mut judged = add(Settled, "them", "other", 9999);
    judged.arbiter_id = Some("me".into());
    all.push(judged);
    for e in all {
        store.insert(e).await.unwrap();
    }

    let list = store.list_for_user("me").await.unwrap();
    let in_play = list.iter().filter(|e| !e.state.is_terminal()).count();
    let ended = list.iter().filter(|e| e.state.is_terminal()).count();
    assert_eq!((in_play, ended), (2, 100), "everything in play, and the newest 100 that ended");
    assert_eq!(list.len(), 102);
    assert!(list.iter().any(|e| e.id == "h001"), "an old escrow still in play is not cut off");
    assert!(list.windows(2).all(|w| w[0].created_at >= w[1].created_at), "newest first");

    let t = store.totals_for_user("me").await.unwrap();
    assert_eq!(t.escrows, 116);
    assert_eq!((t.settled_count, t.settled_paid, t.settled_received), (74, 700, 300));
    assert_eq!((t.refunded_count, t.refunded_sats), (40, 800));
    assert_eq!(store.totals_for_user("nobody").await.unwrap(), Default::default());
}

pub async fn user_store_contract(store: &dyn UserStore) {
    let alice = User { id: "alice".into(), role: Role::User, created_at: 10 };
    let root = User { id: "root".into(), role: Role::Operator, created_at: 11 };
    store.insert_user(alice.clone(), "hash-a".into()).await.unwrap();
    store.insert_user(root.clone(), "hash-r".into()).await.unwrap();

    assert_eq!(store.user_by_token_hash("hash-a").await.unwrap(), Some(alice.clone()));
    assert_eq!(store.user_by_token_hash("hash-r").await.unwrap(), Some(root));
    assert_eq!(store.user_by_token_hash("nope").await.unwrap(), None);
    assert_eq!(store.get_user("alice").await.unwrap(), Some(alice.clone()));
    assert_eq!(store.get_user("ghost").await.unwrap(), None);

    // Ids and token hashes are unique.
    assert_eq!(store.insert_user(alice, "other".into()).await, Err(StoreError::Conflict));
    let bob = User { id: "bob".into(), role: Role::User, created_at: 12 };
    assert_eq!(store.insert_user(bob, "hash-a".into()).await, Err(StoreError::Conflict));

    // API keys: found by hash, listed newest first per user, revoked once and only by their owner.
    let key = |id: &str, user: &str, scopes: Vec<Scope>, at: i64| ApiKey {
        id: id.into(),
        user_id: user.into(),
        name: format!("key {id}"),
        last_chars: "ab12".into(),
        scopes,
        created_at: at,
        last_used_at: None,
        revoked_at: None,
    };
    let k1 = key("k1", "alice", vec![Scope::EscrowsRead], 60);
    let k2 = key("k2", "alice", vec![Scope::EscrowsRead, Scope::EscrowsWrite], 70);
    store.insert_api_key(k1.clone(), "kh-1".into()).await.unwrap();
    store.insert_api_key(k2.clone(), "kh-2".into()).await.unwrap();
    assert_eq!(store.insert_api_key(k1.clone(), "kh-3".into()).await, Err(StoreError::Conflict), "id is unique");
    assert_eq!(
        store.insert_api_key(key("k3", "alice", vec![], 80), "kh-2".into()).await,
        Err(StoreError::Conflict),
        "hash is unique"
    );
    assert_eq!(store.api_key_by_hash("kh-2").await.unwrap(), Some(k2.clone()));
    assert_eq!(store.api_key_by_hash("missing").await.unwrap(), None);
    assert!(k2.allows(Scope::EscrowsWrite) && !k1.allows(Scope::EscrowsWrite));
    assert_eq!(store.list_api_keys("alice").await.unwrap(), vec![k2.clone(), k1.clone()]);
    assert!(store.list_api_keys("root").await.unwrap().is_empty());
    store.touch_api_key("k1", 90).await.unwrap();
    assert_eq!(store.api_key_by_hash("kh-1").await.unwrap().unwrap().last_used_at, Some(90));
    assert!(!store.revoke_api_key("root", "k1", 95).await.unwrap(), "not the owner's");
    assert!(store.revoke_api_key("alice", "k1", 96).await.unwrap());
    assert!(!store.revoke_api_key("alice", "k1", 97).await.unwrap(), "already revoked");
    let revoked = store.api_key_by_hash("kh-1").await.unwrap().unwrap();
    assert_eq!(revoked.revoked_at, Some(96), "the first revocation time stands");
    assert!(!revoked.allows(Scope::EscrowsRead), "a revoked key allows nothing");

    // Webhook endpoints: stored per user, found when due, and their progress recorded.
    let hook = |id: &str, user: &str, at: i64| Webhook {
        id: id.into(),
        user_id: user.into(),
        url: format!("https://{id}.example.com/hook"),
        signing_key_enc: vec![1, 2, 3],
        created_at: at,
        last_seq: 5,
        attempts: 0,
        next_attempt_at: at,
        failing_since: None,
        disabled_at: None,
        last_attempt_at: None,
        last_status: None,
    };
    let (h1, h2) = (hook("h1", "alice", 100), hook("h2", "alice", 200));
    store.insert_webhook(h1.clone()).await.unwrap();
    store.insert_webhook(h2.clone()).await.unwrap();
    assert_eq!(store.insert_webhook(h1.clone()).await, Err(StoreError::Conflict));
    store.insert_webhook(hook("h3", "root", 150)).await.unwrap();
    assert_eq!(store.get_webhook("h1").await.unwrap(), Some(h1.clone()));
    assert_eq!(store.get_webhook("nope").await.unwrap(), None);
    assert_eq!(store.list_webhooks("alice").await.unwrap(), vec![h1.clone(), h2.clone()], "oldest first");
    let due = |now| async move { store.due_webhooks(now, 10).await.unwrap().into_iter().map(|h| h.id).collect::<Vec<_>>() };
    assert_eq!(due(99).await, Vec::<String>::new());
    assert_eq!(due(150).await, vec!["h1", "h3"]);
    assert_eq!(due(300).await, vec!["h1", "h3", "h2"], "most overdue first");
    assert_eq!(store.due_webhooks(300, 2).await.unwrap().len(), 2, "limited");
    let progress = WebhookProgress {
        last_seq: 9,
        attempts: 2,
        next_attempt_at: 500,
        failing_since: Some(400),
        disabled_at: None,
        last_attempt_at: Some(410),
        last_status: Some("503".into()),
    };
    store.record_webhook_progress("h1", progress.clone()).await.unwrap();
    let h = store.get_webhook("h1").await.unwrap().unwrap();
    assert_eq!((h.last_seq, h.attempts, h.next_attempt_at, h.failing_since), (9, 2, 500, Some(400)));
    assert_eq!((h.last_attempt_at, h.last_status.as_deref()), (Some(410), Some("503")));
    assert_eq!(due(300).await, vec!["h3", "h2"], "not due until its next attempt");
    store.record_webhook_progress("h2", WebhookProgress { disabled_at: Some(450), ..progress }).await.unwrap();
    assert_eq!(due(10_000).await, vec!["h3", "h1"], "a disabled endpoint is never due");
    store.record_webhook_progress("ghost", WebhookProgress { last_seq: 1, attempts: 0, next_attempt_at: 0, failing_since: None, disabled_at: None, last_attempt_at: None, last_status: None }).await.unwrap();
    assert!(!store.delete_webhook("root", "h1").await.unwrap(), "not the owner's");
    assert!(store.delete_webhook("alice", "h1").await.unwrap());
    assert!(!store.delete_webhook("alice", "h1").await.unwrap());
    assert_eq!(store.get_webhook("h1").await.unwrap(), None);

    // An arbiter's public key: registered, replaced as a whole, kept per user.
    assert_eq!(store.arbiter_key("alice").await.unwrap(), None);
    store.put_arbiter_key("alice", "key-one", 40).await.unwrap();
    store.put_arbiter_key("root", "key-root", 41).await.unwrap();
    assert_eq!(store.arbiter_key("alice").await.unwrap().as_deref(), Some("key-one"));
    store.put_arbiter_key("alice", "key-two", 50).await.unwrap();
    assert_eq!(store.arbiter_key("alice").await.unwrap().as_deref(), Some("key-two"), "replaced");
    assert_eq!(store.arbiter_key("root").await.unwrap().as_deref(), Some("key-root"));

    // A seller's node connection: saved, replaced as a whole, deleted, kept per user.
    let conn = |user: &str, address: &str, at: i64| NodeConnection {
        user_id: user.into(),
        address: address.into(),
        tls_cert: "-----BEGIN CERTIFICATE-----\nabc\n-----END CERTIFICATE-----".into(),
        macaroon_enc: vec![1, 2, 3, 4],
        updated_at: at,
    };
    assert_eq!(store.node_connection("alice").await.unwrap(), None);
    store.put_node_connection(conn("alice", "https://alice.example:10009", 20)).await.unwrap();
    store.put_node_connection(conn("root", "https://root.example:10009", 21)).await.unwrap();
    assert_eq!(store.node_connection("alice").await.unwrap(), Some(conn("alice", "https://alice.example:10009", 20)));
    store.put_node_connection(conn("alice", "https://alice2.example:10009", 30)).await.unwrap();
    let saved = store.node_connection("alice").await.unwrap().unwrap();
    assert_eq!((saved.address.as_str(), saved.updated_at), ("https://alice2.example:10009", 30), "replaced");
    assert!(store.node_connection("root").await.unwrap().is_some(), "other users are untouched");
    assert!(store.delete_node_connection("alice").await.unwrap());
    assert!(!store.delete_node_connection("alice").await.unwrap(), "nothing left to delete");
    assert_eq!(store.node_connection("alice").await.unwrap(), None);
}

fn withdrawal(id: &str, user: &str, amount: u64, hash: u8, at: i64) -> crate::model::Withdrawal {
    crate::model::Withdrawal {
        id: id.into(),
        user_id: user.into(),
        kind: crate::model::WithdrawalKind::Lightning,
        amount_sats: amount,
        fee_limit_sats: crate::payout::fee_limit(amount),
        fee_sats: None,
        invoice: format!("lnbcrt{id}"),
        payment_hash: [hash; 32],
        state: crate::model::WithdrawalState::Pending,
        failure: None,
        created_at: at,
        updated_at: at,
    }
}

fn ramp_order(id: &str, escrow: Option<&str>, at: i64) -> crate::model::RampOrder {
    use crate::model::{BankAccount, DepositInstructions, RampDirection, RampOrder, RampState};
    RampOrder {
        id: id.into(),
        user_id: "buyer".into(),
        direction: if escrow.is_some() { RampDirection::On } else { RampDirection::Off },
        escrow_id: escrow.map(str::to_string),
        withdrawal_id: escrow.is_none().then(|| "w-1".to_string()),
        amount_sats: 5_000,
        currency: "NGN".into(),
        fiat_minor: 494_813,
        fee_minor: 7_313,
        rate_minor_per_btc: 9_750_000_000,
        bank: BankAccount {
            bank_code: "058".into(),
            bank_name: "Guaranty Trust Bank".into(),
            account_number: "0123456789".into(),
            account_name: "ADA OBI".into(),
        },
        deposit: escrow.map(|_| DepositInstructions {
            bank_name: "Demo Bank".into(),
            account_number: "9912345678".into(),
            account_name: "TOSHI RAMP DEMO".into(),
            reference: "TW-ABCDEF12".into(),
            expires_at: at + 500,
        }),
        provider: "demo".into(),
        fiat_reference: None,
        state: if escrow.is_some() { RampState::AwaitingDeposit } else { RampState::SendingSats },
        note: None,
        created_at: at,
        updated_at: at,
    }
}

/// Behaviour every [`crate::store::PayoutStore`] must provide: balances that come only from settled
/// shared-node escrows, a balance check that cannot be raced, and ramp orders that round-trip.
pub async fn payout_store_contract<S: Store + crate::store::PayoutStore>(store: &S) {
    use crate::{
        model::{BankAccount, RampState, WithdrawalState as W},
        store::{RampUpdate, WithdrawalLimits},
    };
    let none = WithdrawalLimits::NONE;

    assert_eq!(store.ledger_totals("seller", &none).await.unwrap().available(), 0);

    // Only settled escrows on the shared node, where they are the seller, count.
    let mut earned = escrow("earned");
    earned.state = EscrowState::Settled;
    earned.amount_sats = 10_000;
    let mut own = escrow("own-node");
    own.state = EscrowState::Settled;
    own.node = NodeKind::Own;
    let mut locked = escrow("still-locked");
    locked.state = EscrowState::Locked;
    let mut bought = escrow("bought");
    bought.state = EscrowState::Settled;
    bought.seller_id = "someone-else".into();
    bought.buyer_id = Some("seller".into());
    for e in [earned, own, locked, bought] {
        store.insert(e).await.unwrap();
    }
    let t = store.ledger_totals("seller", &none).await.unwrap();
    assert_eq!((t.earned_sats, t.held_sats, t.withdrawn_sats, t.available()), (10_000, 0, 0, 10_000));

    // A pending withdrawal holds its amount and fee allowance (9,000 + 90).
    assert!(store.insert_withdrawal(withdrawal("w1", "seller", 9_000, 1, 200), &none).await.unwrap());
    assert_eq!(store.ledger_totals("seller", &none).await.unwrap().available(), 910);
    // More than what is left is refused, and changes nothing.
    assert!(!store.insert_withdrawal(withdrawal("w2", "seller", 1_000, 2, 201), &none).await.unwrap());
    assert_eq!(store.get_withdrawal("w2").await.unwrap(), None);
    // The same invoice cannot be paid twice, nor the same id reused.
    assert_eq!(store.insert_withdrawal(withdrawal("w3", "seller", 10, 1, 202), &none).await, Err(StoreError::Conflict));
    assert_eq!(store.insert_withdrawal(withdrawal("w1", "seller", 10, 9, 202), &none).await, Err(StoreError::Conflict));
    assert_eq!(store.pending_withdrawals().await.unwrap().len(), 1);

    // Success charges the fee actually paid; it can only finish once.
    assert!(store.finish_withdrawal("w1", W::Succeeded, Some(3), None, 300).await.unwrap());
    assert!(!store.finish_withdrawal("w1", W::Failed, None, Some("late".into()), 301).await.unwrap());
    let w1 = store.get_withdrawal("w1").await.unwrap().unwrap();
    assert_eq!((w1.state, w1.fee_sats, w1.updated_at), (W::Succeeded, Some(3), 300));
    let t = store.ledger_totals("seller", &none).await.unwrap();
    assert_eq!((t.held_sats, t.withdrawn_sats, t.available()), (0, 9_003, 997));

    // A failure gives the sats back. Its invoice is never paid again: a payment thought failed may
    // still go through, so a second try would pay twice. The seller makes a new invoice instead.
    assert!(store.insert_withdrawal(withdrawal("w4", "seller", 500, 4, 400), &none).await.unwrap());
    assert!(store.finish_withdrawal("w4", W::Failed, None, Some("no route".into()), 401).await.unwrap());
    assert_eq!(store.get_withdrawal("w4").await.unwrap().unwrap().failure.as_deref(), Some("no route"));
    assert_eq!(store.ledger_totals("seller", &none).await.unwrap().available(), 997);
    assert_eq!(store.insert_withdrawal(withdrawal("w5", "seller", 500, 4, 500), &none).await, Err(StoreError::Conflict));
    assert!(store.insert_withdrawal(withdrawal("w5", "seller", 500, 5, 500), &none).await.unwrap());
    let ids: Vec<String> = store.list_withdrawals("seller").await.unwrap().into_iter().map(|w| w.id).collect();
    assert_eq!(ids, ["w5", "w4", "w1"], "newest first");
    assert!(store.list_withdrawals("buyer").await.unwrap().is_empty());

    // Ramp orders round-trip, bank details and deposit instructions included.
    let on = ramp_order("r1", Some("earned"), 100);
    store.insert_ramp_order(on.clone()).await.unwrap();
    assert_eq!(store.get_ramp_order("r1").await.unwrap(), Some(on.clone()));
    let off = ramp_order("r2", None, 110);
    store.insert_ramp_order(off.clone()).await.unwrap();
    assert_eq!(store.get_ramp_order("r2").await.unwrap(), Some(off));
    assert_eq!(store.insert_ramp_order(ramp_order("r1", None, 120)).await, Err(StoreError::Conflict));
    // One order in progress per escrow.
    assert_eq!(store.insert_ramp_order(ramp_order("r3", Some("earned"), 130)).await, Err(StoreError::Conflict));

    // Compare-and-set on the state; the note and reference are kept.
    let upd = RampUpdate { note: Some("expired".into()), fiat_reference: Some("REF-1".into()), refund_to: None };
    assert!(!store.update_ramp_order("r1", RampState::Paying, RampState::Expired, upd.clone(), 140).await.unwrap());
    assert!(store.update_ramp_order("r1", RampState::AwaitingDeposit, RampState::Expired, upd, 140).await.unwrap());
    let r1 = store.get_ramp_order("r1").await.unwrap().unwrap();
    assert_eq!((r1.state, r1.note.as_deref(), r1.fiat_reference.as_deref(), r1.updated_at), (RampState::Expired, Some("expired"), Some("REF-1"), 140));
    let keep = RampUpdate::default();
    assert!(store.update_ramp_order("r2", RampState::SendingSats, RampState::PayingOut, keep, 150).await.unwrap());
    let r2 = store.get_ramp_order("r2").await.unwrap().unwrap();
    assert_eq!((r2.note, r2.bank), (None, ramp_order("r2", None, 110).bank), "nothing given, nothing changed");
    // The account a refund goes to can be replaced (by the one the money really came from).
    let payer = BankAccount {
        bank_code: "057".into(),
        bank_name: "Zenith Bank".into(),
        account_number: "2099999999".into(),
        account_name: "PAYER".into(),
    };
    let upd = RampUpdate { refund_to: Some(payer.clone()), ..Default::default() };
    assert!(store.update_ramp_order("r2", RampState::PayingOut, RampState::PayingOut, upd, 155).await.unwrap());
    assert_eq!(store.get_ramp_order("r2").await.unwrap().unwrap().bank, payer);

    // Once the first has ended, the escrow can have a new one, and that is the one found.
    store.insert_ramp_order(ramp_order("r3", Some("earned"), 160)).await.unwrap();
    assert_eq!(store.ramp_order_for_escrow("earned").await.unwrap().unwrap().id, "r3");
    assert_eq!(store.ramp_order_for_escrow("own-node").await.unwrap(), None);
    let open: Vec<String> = store.open_ramp_orders().await.unwrap().into_iter().map(|o| o.id).collect();
    assert_eq!(open.len(), 2);
    assert!(open.contains(&"r2".to_string()) && open.contains(&"r3".to_string()));
    let mine: Vec<String> = store.list_ramp_orders("buyer").await.unwrap().into_iter().map(|o| o.id).collect();
    assert_eq!(mine, ["r3", "r2", "r1"]);

    // Sats from an escrow paid in naira are held back until it has been settled for a while.
    let before = store.ledger_totals("seller", &none).await.unwrap();
    let mut naira = escrow("naira-paid");
    naira.state = EscrowState::Settled;
    naira.amount_sats = 2_000;
    naira.updated_at = 1_000;
    store.insert(naira).await.unwrap();
    store.insert_ramp_order(ramp_order("r9", Some("naira-paid"), 900)).await.unwrap();
    assert!(store.update_ramp_order("r9", RampState::AwaitingDeposit, RampState::Complete, RampUpdate::default(), 1_000).await.unwrap());
    let holding = WithdrawalLimits { matured_before: 999, ..none };
    let t = store.ledger_totals("seller", &holding).await.unwrap();
    assert_eq!((t.earned_sats, t.maturing_sats, t.available()), (before.earned_sats + 2_000, 2_000, before.available()));
    let matured = WithdrawalLimits { matured_before: 1_000, ..none };
    assert_eq!(store.ledger_totals("seller", &matured).await.unwrap().available(), before.available() + 2_000);
    // Maturing sats cannot be withdrawn.
    let all = before.available() + 2_000;
    let big = withdrawal("w6", "seller", all - crate::payout::fee_limit(all) - 1, 6, 1_100);
    assert!(!store.insert_withdrawal(big.clone(), &holding).await.unwrap());
    assert!(store.insert_withdrawal(big.clone(), &matured).await.unwrap());

    // Withdrawals since the window opened count towards its cap, however much balance is left; failed
    // ones don't count.
    let mut capped = escrow("capped-earnings");
    capped.state = EscrowState::Settled;
    capped.seller_id = "capped".into();
    capped.amount_sats = 10_000;
    store.insert(capped).await.unwrap();
    let window = WithdrawalLimits { window_from: 2_000, window_cap_sats: 1_000, ..none };
    assert!(store.insert_withdrawal(withdrawal("c1", "capped", 900, 7, 2_000), &window).await.unwrap());
    assert!(!store.insert_withdrawal(withdrawal("c2", "capped", 200, 8, 2_001), &window).await.unwrap());
    assert_eq!(store.ledger_totals("capped", &window).await.unwrap().withdrawn_in_window_sats, 900);
    assert!(store.finish_withdrawal("c1", W::Failed, None, Some("no route".into()), 2_002).await.unwrap());
    assert!(store.insert_withdrawal(withdrawal("c2", "capped", 200, 8, 2_003), &window).await.unwrap());
    // Earlier withdrawals fall out of the window.
    let later = WithdrawalLimits { window_from: 2_004, ..window };
    assert_eq!(store.ledger_totals("capped", &later).await.unwrap().withdrawn_in_window_sats, 0);

    // A withdrawal PIN: set, wrong guesses counted and then locked, a right guess forgets them,
    // and setting it again starts afresh.
    assert_eq!(store.get_withdrawal_pin("seller").await.unwrap(), None);
    store.withdrawal_pin_failed("nobody", 3, 9_000).await.unwrap();
    store.set_withdrawal_pin("seller", "hash-1", 3_000).await.unwrap();
    let pin = store.get_withdrawal_pin("seller").await.unwrap().unwrap();
    assert_eq!((pin.pin_hash.as_str(), pin.failures, pin.locked_until, pin.updated_at), ("hash-1", 0, None, 3_000));
    store.withdrawal_pin_failed("seller", 3, 9_000).await.unwrap();
    store.withdrawal_pin_failed("seller", 3, 9_000).await.unwrap();
    assert_eq!(store.get_withdrawal_pin("seller").await.unwrap().unwrap().locked_until, None);
    store.withdrawal_pin_failed("seller", 3, 9_000).await.unwrap();
    let pin = store.get_withdrawal_pin("seller").await.unwrap().unwrap();
    assert_eq!((pin.failures, pin.locked_until), (3, Some(9_000)));
    store.withdrawal_pin_ok("seller").await.unwrap();
    let pin = store.get_withdrawal_pin("seller").await.unwrap().unwrap();
    assert_eq!((pin.failures, pin.locked_until), (0, None));
    store.withdrawal_pin_failed("seller", 1, 9_500).await.unwrap();
    store.set_withdrawal_pin("seller", "hash-2", 3_100).await.unwrap();
    let pin = store.get_withdrawal_pin("seller").await.unwrap().unwrap();
    assert_eq!((pin.pin_hash.as_str(), pin.failures, pin.locked_until), ("hash-2", 0, None));
    assert_eq!(store.get_withdrawal_pin("nobody").await.unwrap(), None);
}
