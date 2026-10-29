//! End-to-end behaviour of the escrow core against the mock node.

use std::sync::Arc;

use escrow_core::{
    mock::{MockDirectory, MockLightning},
    adapter::AdapterError,
    model::{Escrow, NodeInvoiceState, NodeKind, Preimage},
    service::{Decision, EscrowService, NewEscrow, ServiceError},
    state::EscrowState,
    store::MemoryStore,
    timing::{TimingConfig, TimingError},
};

const T0: i64 = 1_000_000;
const HOUR: i64 = 3600;

type Svc = EscrowService<MockLightning, Arc<MemoryStore>>;

struct Fixture {
    svc: Svc,
    node: MockLightning,
    store: Arc<MemoryStore>,
}

fn fixture() -> Fixture {
    let node = MockLightning::new();
    let store = Arc::new(MemoryStore::new());
    let svc = EscrowService::new(node.clone(), store.clone(), TimingConfig::default());
    Fixture { svc, node, store }
}

fn secret(n: u8) -> Preimage {
    Preimage::from_bytes([n; 32])
}

fn offer(deadline_at: i64) -> NewEscrow {
    NewEscrow {
        buyer_id: Some("buyer".into()),
        seller_id: "seller".into(),
        arbiter: None,
        fiat: None,
        reference: None,
        return_url: None,
        amount_sats: 1000,
        description: "deliver the parcel".into(),
        deadline_at,
    }
}

/// Create an escrow and issue its invoice.
async fn issued(f: &Fixture, p: &Preimage) -> Escrow {
    let e = f.svc.create(offer(T0 + HOUR), T0).await.unwrap();
    f.svc.accept(&e.id, p.hash(), "buyer", T0).await.unwrap()
}

/// Create, issue and lock an escrow (the buyer pays and the node reports it held).
async fn locked(f: &Fixture, p: &Preimage) -> Escrow {
    let e = issued(f, p).await;
    let ev = f.node.pay(&p.hash());
    f.svc.apply_event(ev, T0 + 5).await.unwrap().unwrap();
    f.svc.get(&e.id).await.unwrap()
}

#[tokio::test]
async fn happy_path_lock_then_settle() {
    let f = fixture();
    let p = secret(1);
    let e = issued(&f, &p).await;
    assert_eq!(e.state, EscrowState::InvoiceIssued);
    assert!(e.invoice.is_some());

    // Not locked until the node says so.
    assert_eq!(f.svc.get(&e.id).await.unwrap().state, EscrowState::InvoiceIssued);
    let ev = f.node.pay(&p.hash());
    let e = f.svc.apply_event(ev, T0 + 5).await.unwrap().unwrap();
    assert_eq!(e.state, EscrowState::Locked);

    let e = f.svc.release(&e.id, &p, "seller", T0 + 600).await.unwrap();
    assert_eq!(e.state, EscrowState::Settled);
    assert_eq!(f.node.state(&p.hash()), Some(NodeInvoiceState::Settled));

    let log = f.svc.audit_log(&e.id).await.unwrap();
    let path: Vec<_> = log.iter().map(|a| (a.from_state, a.to_state)).collect();
    assert_eq!(
        path,
        vec![
            (EscrowState::Created, EscrowState::InvoiceIssued),
            (EscrowState::InvoiceIssued, EscrowState::Locked),
            (EscrowState::Locked, EscrowState::Settled),
        ]
    );
}

#[tokio::test]
async fn wrong_preimage_is_rejected_without_touching_state_or_node() {
    let f = fixture();
    let p = secret(1);
    let e = locked(&f, &p).await;

    let err = f.svc.release(&e.id, &secret(2), "seller", T0 + 10).await.unwrap_err();
    assert!(matches!(err, ServiceError::WrongPreimage));
    assert_eq!(f.svc.get(&e.id).await.unwrap().state, EscrowState::Locked);
    assert_eq!(f.node.settle_calls(), 0);
    assert_eq!(f.svc.audit_log(&e.id).await.unwrap().len(), 2);
}

#[tokio::test]
async fn short_release_codes_cannot_be_parsed_as_preimages() {
    assert!(Preimage::from_hex("123456").is_none());
    assert!(Preimage::from_hex(&"ab".repeat(31)).is_none());
    assert!(Preimage::from_hex(&"ab".repeat(33)).is_none());
    assert!(Preimage::from_hex(&"zz".repeat(32)).is_none());
    assert!(Preimage::from_hex(&"ab".repeat(32)).is_some());
}

#[tokio::test]
async fn duplicate_release_does_not_settle_twice() {
    let f = fixture();
    let p = secret(1);
    let e = locked(&f, &p).await;

    f.svc.release(&e.id, &p, "seller", T0 + 10).await.unwrap();
    let again = f.svc.release(&e.id, &p, "seller", T0 + 11).await.unwrap();
    assert_eq!(again.state, EscrowState::Settled);
    assert_eq!(f.node.settle_calls(), 1);
    assert_eq!(f.svc.audit_log(&e.id).await.unwrap().len(), 3);
}

#[tokio::test]
async fn deadline_passes_and_the_scheduler_refunds() {
    let f = fixture();
    let p = secret(1);
    let e = locked(&f, &p).await;

    let early = f.svc.tick(T0 + HOUR - 1).await.unwrap();
    assert!(early.cancelled.is_empty());
    assert_eq!(f.svc.get(&e.id).await.unwrap().state, EscrowState::Locked);

    let report = f.svc.tick(T0 + HOUR).await.unwrap();
    assert_eq!(report.cancelled, vec![e.id.clone()]);
    assert_eq!(f.svc.get(&e.id).await.unwrap().state, EscrowState::Refunded);
    assert_eq!(f.node.state(&p.hash()), Some(NodeInvoiceState::Canceled));

    // A second pass does nothing more.
    let again = f.svc.tick(T0 + HOUR + 60).await.unwrap();
    assert!(again.cancelled.is_empty());
    assert_eq!(f.node.cancel_calls(), 1);
}

#[tokio::test]
async fn release_after_the_deadline_is_too_late() {
    let f = fixture();
    let p = secret(1);
    let e = locked(&f, &p).await;

    let err = f.svc.release(&e.id, &p, "seller", T0 + HOUR).await.unwrap_err();
    assert!(matches!(err, ServiceError::TooLate));
    assert_eq!(f.node.settle_calls(), 0);
    assert_eq!(f.svc.get(&e.id).await.unwrap().state, EscrowState::Locked);
}

#[tokio::test]
async fn node_auto_cancel_is_recorded_as_a_refund() {
    let f = fixture();
    let p = secret(1);
    let _ = locked(&f, &p).await;

    let ev = f.node.auto_cancel(&p.hash());
    let e = f.svc.apply_event(ev, T0 + 100).await.unwrap().unwrap();
    assert_eq!(e.state, EscrowState::Refunded);
}

#[tokio::test]
async fn unpaid_invoice_expires() {
    let f = fixture();
    let p = secret(1);
    let _ = issued(&f, &p).await;

    let ev = f.node.expire_unpaid(&p.hash());
    let e = f.svc.apply_event(ev, T0 + 601).await.unwrap().unwrap();
    assert_eq!(e.state, EscrowState::ExpiredUnpaid);
}

#[tokio::test]
async fn scheduler_expires_an_unpaid_invoice_the_node_never_reported() {
    let f = fixture();
    let p = secret(1);
    let e = issued(&f, &p).await;

    let report = f.svc.tick(T0 + 600).await.unwrap();
    assert_eq!(report.cancelled, vec![e.id.clone()]);
    assert_eq!(f.svc.get(&e.id).await.unwrap().state, EscrowState::ExpiredUnpaid);
}

#[tokio::test]
async fn node_offline_at_settle_time_leaves_the_escrow_locked_and_retryable() {
    let f = fixture();
    let p = secret(1);
    let e = locked(&f, &p).await;

    f.node.set_offline(true);
    let err = f.svc.release(&e.id, &p, "seller", T0 + 10).await.unwrap_err();
    assert!(matches!(err, ServiceError::Adapter(_)));
    assert_eq!(f.svc.get(&e.id).await.unwrap().state, EscrowState::Locked);

    f.node.set_offline(false);
    let e = f.svc.release(&e.id, &p, "seller", T0 + 20).await.unwrap();
    assert_eq!(e.state, EscrowState::Settled);
}

#[tokio::test]
async fn restart_mid_escrow_recovers_state_from_the_node() {
    let f = fixture();
    let p = secret(1);
    let e = issued(&f, &p).await;

    // The buyer pays while the backend is down, so the event is missed.
    f.node.pay(&p.hash());
    assert_eq!(f.svc.get(&e.id).await.unwrap().state, EscrowState::InvoiceIssued);

    // A fresh service over the same store and node reconciles on startup.
    let restarted: Svc = EscrowService::new(f.node.clone(), f.store.clone(), TimingConfig::default());
    let report = restarted.reconcile(T0 + 30).await.unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.changed, vec![e.id.clone()]);
    assert!(report.errors.is_empty());
    assert_eq!(restarted.get(&e.id).await.unwrap().state, EscrowState::Locked);

    // Reconciling again changes nothing.
    assert!(restarted.reconcile(T0 + 31).await.unwrap().changed.is_empty());
}

#[tokio::test]
async fn reconcile_adopts_a_settlement_that_happened_while_down() {
    let f = fixture();
    let p = secret(1);
    let e = locked(&f, &p).await;

    // Settled on the node, but the backend never heard.
    use escrow_core::adapter::LightningAdapter;
    f.node.settle_invoice(&p).await.unwrap();
    assert_eq!(f.svc.get(&e.id).await.unwrap().state, EscrowState::Locked);

    f.svc.reconcile(T0 + 30).await.unwrap();
    assert_eq!(f.svc.get(&e.id).await.unwrap().state, EscrowState::Settled);
}

#[tokio::test]
async fn deadline_reached_while_a_dispute_is_open_still_refunds() {
    let f = fixture();
    let p = secret(1);
    let e = locked(&f, &p).await;

    let e = f.svc.dispute(&e.id, "buyer", "goods not as described", T0 + 100).await.unwrap();
    assert_eq!(e.state, EscrowState::Disputed);
    // Repeating the flag is harmless.
    f.svc.dispute(&e.id, "buyer", "again", T0 + 101).await.unwrap();
    assert_eq!(f.svc.audit_log(&e.id).await.unwrap().len(), 3);

    f.svc.tick(T0 + HOUR).await.unwrap();
    assert_eq!(f.svc.get(&e.id).await.unwrap().state, EscrowState::Refunded);
}

#[tokio::test]
async fn a_dispute_cannot_be_raised_after_the_deadline() {
    let f = fixture();
    let p = secret(1);
    let e = locked(&f, &p).await;

    let err = f.svc.dispute(&e.id, "buyer", "late", T0 + HOUR).await.unwrap_err();
    assert!(matches!(err, ServiceError::TooLate));
}

#[tokio::test]
async fn actions_are_rejected_in_the_wrong_state() {
    let f = fixture();
    let p = secret(1);

    // Nothing to release before the funds are locked.
    let e = issued(&f, &p).await;
    let err = f.svc.release(&e.id, &p, "seller", T0 + 1).await.unwrap_err();
    assert!(matches!(err, ServiceError::InvalidState { state: EscrowState::InvoiceIssued, .. }));
    assert_eq!(f.node.settle_calls(), 0);

    // A settled escrow cannot be cancelled or disputed.
    let f = fixture();
    let e = locked(&f, &p).await;
    f.svc.release(&e.id, &p, "seller", T0 + 10).await.unwrap();
    let err = f.svc.cancel(&e.id, "buyer", "changed my mind", T0 + 11).await.unwrap_err();
    assert!(matches!(err, ServiceError::InvalidState { state: EscrowState::Settled, .. }));
    let err = f.svc.dispute(&e.id, "buyer", "too late", T0 + 12).await.unwrap_err();
    assert!(matches!(err, ServiceError::InvalidState { state: EscrowState::Settled, .. }));
}

#[tokio::test]
async fn late_node_events_cannot_move_a_terminal_escrow() {
    let f = fixture();
    let p = secret(1);
    let e = locked(&f, &p).await;
    f.svc.release(&e.id, &p, "seller", T0 + 10).await.unwrap();

    use escrow_core::model::InvoiceEvent;
    let stale = InvoiceEvent { payment_hash: p.hash(), state: NodeInvoiceState::Canceled };
    let e = f.svc.apply_event(stale, T0 + 20).await.unwrap().unwrap();
    assert_eq!(e.state, EscrowState::Settled);
}

#[tokio::test]
async fn impossible_node_state_sends_the_escrow_to_failed() {
    let f = fixture();
    let p = secret(1);
    let e = issued(&f, &p).await;

    use escrow_core::model::InvoiceEvent;
    let bogus = InvoiceEvent { payment_hash: p.hash(), state: NodeInvoiceState::Settled };
    let e2 = f.svc.apply_event(bogus, T0 + 1).await.unwrap().unwrap();
    assert_eq!(e2.id, e.id);
    assert_eq!(e2.state, EscrowState::Failed);
}

#[tokio::test]
async fn repeated_accept_is_idempotent_and_hash_reuse_is_refused() {
    let f = fixture();
    let p = secret(1);
    let e = issued(&f, &p).await;

    let again = f.svc.accept(&e.id, p.hash(), "buyer", T0 + 1).await.unwrap();
    assert_eq!(again, e);
    assert_eq!(f.svc.audit_log(&e.id).await.unwrap().len(), 1);

    // A different hash on an escrow that already has an invoice is refused.
    let err = f.svc.accept(&e.id, secret(2).hash(), "buyer", T0 + 2).await.unwrap_err();
    assert!(matches!(err, ServiceError::InvalidState { .. }));

    // Another escrow cannot reuse the same hash.
    let other = f.svc.create(offer(T0 + HOUR), T0).await.unwrap();
    let err = f.svc.accept(&other.id, p.hash(), "buyer", T0).await.unwrap_err();
    assert!(matches!(err, ServiceError::HashInUse));
}

#[tokio::test]
async fn creation_validates_amount_parties_and_deadline() {
    let f = fixture();

    let mut bad = offer(T0 + HOUR);
    bad.amount_sats = 0;
    assert!(matches!(f.svc.create(bad, T0).await, Err(ServiceError::Invalid(_))));

    let mut bad = offer(T0 + HOUR);
    bad.seller_id = bad.buyer_id.clone().unwrap();
    assert!(matches!(f.svc.create(bad, T0).await, Err(ServiceError::Invalid(_))));

    let err = f.svc.create(offer(T0 + 7 * HOUR), T0).await.unwrap_err();
    assert!(matches!(err, ServiceError::Timing(TimingError::TooLong { .. })));
    let err = f.svc.create(offer(T0 + 5), T0).await.unwrap_err();
    assert!(matches!(err, ServiceError::Timing(TimingError::TooSoon { .. })));
}

#[tokio::test]
async fn the_preimage_never_appears_in_debug_output() {
    let p = Preimage::from_bytes([0xab; 32]);
    let shown = format!("{p:?}");
    assert!(!shown.contains("abab"));
    assert!(shown.contains("redacted"));

    let f = fixture();
    let e = locked(&f, &p).await;
    f.svc.release(&e.id, &p, "seller", T0 + 10).await.unwrap();
    let dump = format!("{:?} {:?}", f.svc.get(&e.id).await.unwrap(), f.svc.audit_log(&e.id).await.unwrap());
    assert!(!dump.contains(&hex::encode([0xab; 32])));
}

#[tokio::test]
async fn memory_store_meets_the_store_contract() {
    escrow_core::testing::store_contract(&MemoryStore::new()).await;
}

#[tokio::test]
async fn memory_store_lists_a_long_history_and_totals_all_of_it() {
    escrow_core::testing::history_contract(&MemoryStore::new()).await;
}

#[tokio::test]
async fn memory_store_meets_the_user_store_contract() {
    escrow_core::testing::user_store_contract(&MemoryStore::new()).await;
}

#[tokio::test]
async fn node_events_are_recorded_for_the_audit_trail() {
    let f = fixture();
    let p = secret(1);
    let e = issued(&f, &p).await;
    let ev = f.node.pay(&p.hash());
    f.svc.apply_event(ev, T0 + 5).await.unwrap();

    use escrow_core::store::Store;
    let events = f.store.node_events(&e.id).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].node_invoice_state, NodeInvoiceState::Accepted);
    assert_eq!(events[0].received_at, T0 + 5);
}

#[tokio::test]
async fn the_first_user_to_accept_an_open_offer_becomes_its_buyer() {
    let f = fixture();
    let open = NewEscrow { buyer_id: None, ..offer(T0 + HOUR) };
    let e = f.svc.create(open, T0).await.unwrap();
    assert_eq!(e.buyer_id, None);

    // The seller cannot be the buyer of their own offer.
    assert!(matches!(f.svc.accept(&e.id, secret(1).hash(), "seller", T0).await, Err(ServiceError::Invalid(_))));
    assert_eq!(f.svc.get(&e.id).await.unwrap().state, EscrowState::Created);

    let e = f.svc.accept(&e.id, secret(1).hash(), "alice", T0).await.unwrap();
    assert_eq!((e.state, e.buyer_id.as_deref()), (EscrowState::InvoiceIssued, Some("alice")));

    // Alice repeating it is fine; someone else is turned away and nothing changes.
    f.svc.accept(&e.id, secret(1).hash(), "alice", T0).await.unwrap();
    assert!(matches!(f.svc.accept(&e.id, secret(2).hash(), "bob", T0).await, Err(ServiceError::Invalid(_))));
    assert_eq!(f.svc.get(&e.id).await.unwrap().buyer_id.as_deref(), Some("alice"));
}

#[tokio::test]
async fn a_named_buyer_cannot_be_replaced_by_someone_else_accepting() {
    let f = fixture();
    let e = f.svc.create(offer(T0 + HOUR), T0).await.unwrap();
    assert!(matches!(f.svc.accept(&e.id, secret(1).hash(), "mallory", T0).await, Err(ServiceError::Invalid(_))));
    assert_eq!(f.node.state(&secret(1).hash()), None, "no invoice was issued");
}

fn offer_from(seller: &str) -> NewEscrow {
    NewEscrow { seller_id: seller.into(), ..offer(T0 + HOUR) }
}

#[tokio::test]
async fn each_sellers_escrow_lives_on_that_sellers_own_node() {
    let (alice_node, bob_node, shared) = (MockLightning::new(), MockLightning::new(), MockLightning::new());
    let dir = MockDirectory::new(Some(shared.clone()));
    dir.connect_seller("alice", alice_node.clone());
    dir.connect_seller("bob", bob_node.clone());
    let svc = EscrowService::new(dir, Arc::new(MemoryStore::new()), TimingConfig::default());

    let (pa, pb, pc) = (secret(1), secret(2), secret(3));
    let a = svc.create(offer_from("alice"), T0).await.unwrap();
    let b = svc.create(offer_from("bob"), T0).await.unwrap();
    let c = svc.create(offer_from("carol"), T0).await.unwrap(); // no node of her own
    assert_eq!((a.node, b.node, c.node), (NodeKind::Own, NodeKind::Own, NodeKind::Shared));

    svc.accept(&a.id, pa.hash(), "buyer", T0).await.unwrap();
    svc.accept(&b.id, pb.hash(), "buyer", T0).await.unwrap();
    svc.accept(&c.id, pc.hash(), "buyer", T0).await.unwrap();

    // Each invoice exists on its own seller's node and on no other.
    for (hash, on) in [(pa.hash(), "alice"), (pb.hash(), "bob"), (pc.hash(), "shared")] {
        let nodes = [("alice", &alice_node), ("bob", &bob_node), ("shared", &shared)];
        for (name, node) in nodes {
            assert_eq!(node.state(&hash).is_some(), name == on, "invoice for {on} on node {name}");
        }
    }

    // Paying and releasing alice's escrow settles on alice's node only.
    let ev = alice_node.pay(&pa.hash());
    svc.apply_event(ev, T0 + 1).await.unwrap();
    let done = svc.release(&a.id, &pa, "alice", T0 + 2).await.unwrap();
    assert_eq!(done.state, EscrowState::Settled);
    assert_eq!((alice_node.settle_calls(), bob_node.settle_calls(), shared.settle_calls()), (1, 0, 0));
    assert_eq!(alice_node.state(&pa.hash()), Some(NodeInvoiceState::Settled));
}

#[tokio::test]
async fn a_seller_with_no_node_cannot_create_an_escrow_when_there_is_no_shared_node() {
    let dir = MockDirectory::new(None);
    dir.connect_seller("alice", MockLightning::new());
    let svc = EscrowService::new(dir, Arc::new(MemoryStore::new()), TimingConfig::default());
    assert!(svc.create(offer_from("alice"), T0).await.is_ok());
    let err = svc.create(offer_from("bob"), T0).await.unwrap_err();
    assert!(matches!(err, ServiceError::Adapter(AdapterError::NoNode(_))), "{err}");
}

#[tokio::test]
async fn one_sellers_node_going_down_does_not_affect_another_sellers_escrows() {
    let (alice_node, bob_node) = (MockLightning::new(), MockLightning::new());
    let dir = MockDirectory::new(None);
    dir.connect_seller("alice", alice_node.clone());
    dir.connect_seller("bob", bob_node.clone());
    let svc = EscrowService::new(dir, Arc::new(MemoryStore::new()), TimingConfig::default());
    let (pa, pb) = (secret(1), secret(2));
    let a = svc.create(offer_from("alice"), T0).await.unwrap();
    let b = svc.create(offer_from("bob"), T0).await.unwrap();
    svc.accept(&a.id, pa.hash(), "buyer", T0).await.unwrap();
    svc.accept(&b.id, pb.hash(), "buyer", T0).await.unwrap();
    for (node, p) in [(&alice_node, &pa), (&bob_node, &pb)] {
        let ev = node.pay(&p.hash());
        svc.apply_event(ev, T0 + 1).await.unwrap();
    }

    alice_node.set_offline(true);
    assert!(matches!(svc.release(&a.id, &pa, "alice", T0 + 2).await, Err(ServiceError::Adapter(_))));
    assert_eq!(svc.get(&a.id).await.unwrap().state, EscrowState::Locked, "alice's escrow is untouched");
    assert_eq!(svc.release(&b.id, &pb, "bob", T0 + 2).await.unwrap().state, EscrowState::Settled);

    // The scheduler keeps going past a node it cannot reach.
    let report = svc.tick(T0 + HOUR + 1).await.unwrap();
    assert_eq!(report.errors.len(), 1, "only alice's escrow could not be cancelled");
    alice_node.set_offline(false);
    svc.tick(T0 + HOUR + 2).await.unwrap();
    assert_eq!(svc.get(&a.id).await.unwrap().state, EscrowState::Refunded);
}

// --- Arbitration -------------------------------------------------------------------------------

/// Stand-in for the buyer's secret encrypted to the arbiter: the right shape, opaque to the service.
const SEALED: &str = "BAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+P0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fYGFiY2RlZmdoaWprbG1ub3A=";

fn arbitrated(buyer: Option<&str>) -> NewEscrow {
    NewEscrow {
        buyer_id: buyer.map(str::to_string),
        arbiter: Some(("judge".into(), "judge-public-key".into())),
        ..offer(T0 + HOUR)
    }
}

/// Create an arbitrated escrow, accept it with a sealed secret, and lock it.
async fn arbitrated_locked(f: &Fixture, p: &Preimage) -> Escrow {
    let e = f.svc.create(arbitrated(Some("buyer")), T0).await.unwrap();
    f.svc.accept_with(&e.id, p.hash(), "buyer", T0, Some(SEALED.into())).await.unwrap();
    let ev = f.node.pay(&p.hash());
    f.svc.apply_event(ev, T0 + 5).await.unwrap();
    f.svc.get(&e.id).await.unwrap()
}

#[tokio::test]
async fn an_arbitrated_escrow_needs_the_sealed_secret_and_no_other_does() {
    let f = fixture();
    let e = f.svc.create(arbitrated(Some("buyer")), T0).await.unwrap();
    assert_eq!((e.arbiter_id.as_deref(), e.arbiter_key.as_deref()), (Some("judge"), Some("judge-public-key")));

    // Without it the escrow cannot be accepted, and no invoice is made.
    let err = f.svc.accept(&e.id, secret(1).hash(), "buyer", T0).await.unwrap_err();
    assert!(matches!(err, ServiceError::Invalid(_)), "{err}");
    assert_eq!(f.node.state(&secret(1).hash()), None);
    // Nonsense is refused too.
    for bad in ["", "short", &"!".repeat(200), &"A".repeat(2000)] {
        let err = f.svc.accept_with(&e.id, secret(1).hash(), "buyer", T0, Some(bad.to_string())).await.unwrap_err();
        assert!(matches!(err, ServiceError::Invalid(_)), "{bad:.20}");
    }

    let e = f.svc.accept_with(&e.id, secret(1).hash(), "buyer", T0, Some(SEALED.into())).await.unwrap();
    assert_eq!(e.arbiter_sealed.as_deref(), Some(SEALED));

    // An escrow with no arbiter has nothing to seal the secret to.
    let plain = f.svc.create(offer(T0 + HOUR), T0).await.unwrap();
    let err = f.svc.accept_with(&plain.id, secret(2).hash(), "buyer", T0, Some(SEALED.into())).await.unwrap_err();
    assert!(matches!(err, ServiceError::Invalid(_)));
}

#[tokio::test]
async fn the_arbiter_cannot_be_a_party() {
    let f = fixture();
    let mut as_seller = arbitrated(Some("buyer"));
    as_seller.arbiter = Some(("seller".into(), "k".into()));
    assert!(matches!(f.svc.create(as_seller, T0).await, Err(ServiceError::Invalid(_))));
    let mut as_buyer = arbitrated(Some("buyer"));
    as_buyer.arbiter = Some(("buyer".into(), "k".into()));
    assert!(matches!(f.svc.create(as_buyer, T0).await, Err(ServiceError::Invalid(_))));
}

#[tokio::test]
async fn the_arbiter_confirms_once_and_only_the_arbiter_can() {
    let f = fixture();
    let p = secret(1);
    let e = f.svc.create(arbitrated(Some("buyer")), T0).await.unwrap();
    // Nothing to confirm before the buyer has sealed a secret.
    assert!(matches!(f.svc.confirm_arbiter(&e.id, "judge", T0).await, Err(ServiceError::InvalidState { .. })));
    f.svc.accept_with(&e.id, p.hash(), "buyer", T0, Some(SEALED.into())).await.unwrap();

    for imposter in ["buyer", "seller", "stranger"] {
        assert!(matches!(f.svc.confirm_arbiter(&e.id, imposter, T0 + 1).await, Err(ServiceError::Invalid(_))));
    }
    assert_eq!(f.svc.get(&e.id).await.unwrap().arbiter_confirmed_at, None);

    let confirmed = f.svc.confirm_arbiter(&e.id, "judge", T0 + 2).await.unwrap();
    assert_eq!((confirmed.state, confirmed.arbiter_confirmed_at), (EscrowState::InvoiceIssued, Some(T0 + 2)));
    // Repeating is a no-op: same timestamp, one audit entry.
    let again = f.svc.confirm_arbiter(&e.id, "judge", T0 + 9).await.unwrap();
    assert_eq!(again.arbiter_confirmed_at, Some(T0 + 2));
    let confirms = f.svc.audit_log(&e.id).await.unwrap().iter().filter(|a| a.action == "arbiter_confirm").count();
    assert_eq!(confirms, 1);
}

#[tokio::test]
async fn the_arbiter_can_release_a_disputed_escrow_to_the_seller() {
    let f = fixture();
    let p = secret(1);
    let e = arbitrated_locked(&f, &p).await;
    f.svc.dispute(&e.id, "seller", "buyer has the goods and will not release", T0 + 10).await.unwrap();
    // The buyer answers, which ends the arbiter's wait.
    f.svc.add_note(&e.id, "buyer", "evidence", "I have not received it", T0 + 12).await.unwrap();

    // A wrong secret changes nothing.
    let wrong = f.svc.arbitrate(&e.id, "judge", Decision::Release(secret(9)), "", T0 + 20).await;
    assert!(matches!(wrong, Err(ServiceError::WrongPreimage)));
    assert_eq!(f.svc.get(&e.id).await.unwrap().state, EscrowState::Disputed);
    let recorded = f.svc.audit_log(&e.id).await.unwrap().iter().filter(|a| a.action == "arbiter_decision").count();
    assert_eq!(recorded, 0, "a rejected attempt leaves no decision on the record");

    let done = f.svc.arbitrate(&e.id, "judge", Decision::Release(p), "delivery photo is genuine", T0 + 30).await.unwrap();
    assert_eq!(done.state, EscrowState::Settled);
    assert_eq!(f.node.state(&secret(1).hash()), Some(NodeInvoiceState::Settled));
    let decisions: Vec<_> =
        f.svc.audit_log(&e.id).await.unwrap().into_iter().filter(|a| a.action == "arbiter_decision").collect();
    assert_eq!(decisions.len(), 1, "the decision is on record exactly once");
    assert_eq!(decisions[0].actor, "judge");
    assert!(
        decisions[0].reason.contains("release to the seller") && decisions[0].reason.contains("delivery photo is genuine"),
        "{}",
        decisions[0].reason
    );

    // Deciding the same thing again does nothing more; deciding the opposite is refused.
    assert!(f.svc.arbitrate(&e.id, "judge", Decision::Release(secret(1)), "", T0 + 40).await.is_ok());
    assert_eq!(f.node.settle_calls(), 1);
    assert!(matches!(f.svc.arbitrate(&e.id, "judge", Decision::Refund, "", T0 + 41).await, Err(ServiceError::InvalidState { .. })));
}

#[tokio::test]
async fn the_arbiter_can_refund_a_disputed_escrow_to_the_buyer() {
    let f = fixture();
    let p = secret(1);
    let e = arbitrated_locked(&f, &p).await;
    f.svc.dispute(&e.id, "buyer", "nothing arrived", T0 + 10).await.unwrap();
    f.svc.add_note(&e.id, "seller", "evidence", "courier receipt NG123", T0 + 12).await.unwrap();
    let done = f.svc.arbitrate(&e.id, "judge", Decision::Refund, "no proof of delivery", T0 + 30).await.unwrap();
    assert_eq!(done.state, EscrowState::Refunded);
    assert_eq!(f.node.state(&p.hash()), Some(NodeInvoiceState::Canceled));
    assert!(f.svc.arbitrate(&e.id, "judge", Decision::Refund, "", T0 + 31).await.is_ok(), "repeat is a no-op");
    assert!(matches!(
        f.svc.arbitrate(&e.id, "judge", Decision::Release(p), "", T0 + 32).await,
        Err(ServiceError::InvalidState { .. })
    ));
}

#[tokio::test]
async fn only_the_arbiter_decides_and_only_a_disputed_escrow_before_the_deadline() {
    let f = fixture();
    let p = secret(1);
    let e = arbitrated_locked(&f, &p).await;

    // Not disputed yet: the arbiter has no say.
    assert!(matches!(f.svc.arbitrate(&e.id, "judge", Decision::Refund, "", T0 + 10).await, Err(ServiceError::InvalidState { .. })));
    f.svc.dispute(&e.id, "buyer", "late", T0 + 11).await.unwrap();

    // Nobody else can decide, whatever they hold.
    for imposter in ["buyer", "seller", "stranger"] {
        assert!(matches!(f.svc.arbitrate(&e.id, imposter, Decision::Refund, "", T0 + 12).await, Err(ServiceError::Invalid(_))));
        assert!(matches!(f.svc.arbitrate(&e.id, imposter, Decision::Release(secret(1)), "", T0 + 12).await, Err(ServiceError::Invalid(_))));
    }
    assert_eq!(f.svc.get(&e.id).await.unwrap().state, EscrowState::Disputed);

    // At the deadline the hold cannot be extended: the arbiter is too late and the scheduler refunds.
    assert!(matches!(f.svc.arbitrate(&e.id, "judge", Decision::Release(p), "", T0 + HOUR).await, Err(ServiceError::TooLate)));
    f.svc.tick(T0 + HOUR).await.unwrap();
    assert_eq!(f.svc.get(&e.id).await.unwrap().state, EscrowState::Refunded);
}

#[test]
fn the_response_window_is_a_quarter_of_the_time_left_and_never_more_than_an_hour() {
    use escrow_core::service::{response_window_secs, RESPONSE_WINDOW_MAX_SECS};
    assert_eq!(RESPONSE_WINDOW_MAX_SECS, 3600);
    assert_eq!(response_window_secs(0, 400), 100);
    assert_eq!(response_window_secs(0, 3600), 900);
    assert_eq!(response_window_secs(0, 14_400), 3600, "a quarter of 4 hours is exactly the cap");
    assert_eq!(response_window_secs(0, 21_600), 3600, "capped at an hour");
    assert_eq!(response_window_secs(100, 100), 0, "no time left, no window");
    assert_eq!(response_window_secs(200, 100), 0, "a deadline already passed gives no window");
}

#[tokio::test]
async fn an_arbiter_waits_for_the_other_side_to_answer_a_dispute_unless_they_already_have() {
    let f = fixture();
    let p = secret(1);
    let e = arbitrated_locked(&f, &p).await;

    // No dispute, no wait to speak of.
    assert_eq!(f.svc.decide_after(&e).await.unwrap(), None);

    // The seller disputes at T0+10 with 3590 s left: the buyer has a quarter of that, 897 s.
    let disputed = f.svc.dispute(&e.id, "seller", "buyer has the goods", T0 + 10).await.unwrap();
    assert_eq!(f.svc.decide_after(&disputed).await.unwrap(), Some(T0 + 10 + 897));

    // The seller's own notes do not end the buyer's window.
    f.svc.add_note(&e.id, "seller", "evidence", "tracking NG123", T0 + 20).await.unwrap();
    assert_eq!(f.svc.decide_after(&disputed).await.unwrap(), Some(T0 + 907));

    // Deciding early is refused, with the time it opens, and leaves nothing on the record.
    let early = f.svc.arbitrate(&e.id, "judge", Decision::Refund, "", T0 + 906).await;
    assert!(matches!(early, Err(ServiceError::TooEarly(t)) if t == T0 + 907), "{early:?}");
    assert_eq!(f.svc.get(&e.id).await.unwrap().state, EscrowState::Disputed);
    assert_eq!(f.svc.audit_log(&e.id).await.unwrap().iter().filter(|a| a.action == "arbiter_decision").count(), 0);

    // The buyer answering ends the wait at once, even though the clock has not moved.
    f.svc.add_note(&e.id, "buyer", "evidence", "the box was empty", T0 + 30).await.unwrap();
    assert_eq!(f.svc.decide_after(&disputed).await.unwrap(), None);
    assert!(f.svc.arbitrate(&e.id, "judge", Decision::Refund, "box was empty", T0 + 31).await.is_ok());
}

#[tokio::test]
async fn a_silent_other_side_cannot_hold_the_decision_past_the_window_or_the_deadline() {
    let f = fixture();
    let e = arbitrated_locked(&f, &secret(1)).await;
    f.svc.dispute(&e.id, "buyer", "nothing arrived", T0 + 10).await.unwrap();
    // Silence: at the end of the window the arbiter may decide.
    assert!(f.svc.arbitrate(&e.id, "judge", Decision::Refund, "seller never answered", T0 + 907).await.is_ok());

    // The window always ends before the deadline, so there is time left to decide.
    let g = fixture();
    let e = arbitrated_locked(&g, &secret(2)).await;
    g.svc.dispute(&e.id, "buyer", "nothing arrived", T0 + 3000).await.unwrap();
    let at = g.svc.decide_after(&g.svc.get(&e.id).await.unwrap()).await.unwrap().unwrap();
    assert!(at < T0 + HOUR, "{at} is before the deadline {}", T0 + HOUR);
}

