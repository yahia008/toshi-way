//! Runs the shared store contract, a restart and a race against a real PostgreSQL.
//! Ignored by default; start the database and run:
//!
//!   docker compose -f regtest/docker-compose.yml --profile postgres up -d postgres
//!   cargo test -p escrow-postgres -- --ignored --test-threads=1
//!
//! The tests wipe the database they connect to, so point `TOSHI_TEST_DATABASE_URL` only at a scratch one.

use std::sync::Arc;

use escrow_core::{
    store::WithdrawalLimits,
    mock::MockLightning,
    model::{AuditEntry, Escrow, Preimage},
    service::{EscrowService, NewEscrow},
    state::EscrowState,
    store::{EscrowUpdate, Store, StoreError},
    testing::{store_contract, user_store_contract},
    timing::TimingConfig,
};
use escrow_postgres::PostgresStore;

const T0: i64 = 1_000_000;

fn url() -> String {
    std::env::var("TOSHI_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://toshi:toshi@127.0.0.1:55432/toshi".into())
}

async fn fresh() -> PostgresStore {
    let store = PostgresStore::connect(&url()).await.expect("is the postgres profile running?");
    store.wipe_for_tests().await.unwrap();
    store
}

#[tokio::test]
#[ignore = "needs PostgreSQL"]
async fn postgres_store_meets_the_store_contract() {
    store_contract(&fresh().await).await;
}

#[tokio::test]
#[ignore = "needs PostgreSQL"]
async fn postgres_store_lists_a_long_history_and_totals_all_of_it() {
    escrow_core::testing::history_contract(&fresh().await).await;
}

#[tokio::test]
#[ignore = "needs PostgreSQL"]
async fn postgres_store_meets_the_user_store_contract() {
    user_store_contract(&fresh().await).await;
}

#[tokio::test]
#[ignore = "needs PostgreSQL"]
async fn connecting_again_does_not_re_run_migrations_or_lose_data() {
    let store = fresh().await;
    let svc = EscrowService::new(MockLightning::new(), Arc::new(store), TimingConfig::default());
    let e = svc
        .create(
            NewEscrow {
                buyer_id: None,
                arbiter: None,
                fiat: None,
                reference: None,
                return_url: None,
                seller_id: "seller".into(),
                amount_sats: 1000,
                description: "open offer".into(),
                deadline_at: T0 + 3600,
            },
            T0,
        )
        .await
        .unwrap();
    drop(svc);

    let again = PostgresStore::connect(&url()).await.unwrap();
    let kept: Escrow = again.get(&e.id).await.unwrap().expect("data survived the reconnect");
    assert_eq!(kept.buyer_id, None, "open offers are supported");
}

/// Two actors race to move the same escrow (a node event and the scheduler, say): exactly one wins.
#[tokio::test]
#[ignore = "needs PostgreSQL"]
async fn only_one_of_two_concurrent_transitions_wins() {
    let store = Arc::new(fresh().await);
    let svc = EscrowService::new(MockLightning::new(), store.clone(), TimingConfig::default());
    let p = Preimage::from_bytes([3; 32]);
    let e = svc
        .create(
            NewEscrow {
                buyer_id: Some("buyer".into()),
        arbiter: None,
        fiat: None,
        reference: None,
        return_url: None,
                seller_id: "seller".into(),
                amount_sats: 1000,
                description: "race".into(),
                deadline_at: T0 + 3600,
            },
            T0,
        )
        .await
        .unwrap();
    svc.accept(&e.id, p.hash(), "buyer", T0).await.unwrap();

    let audit = |to: EscrowState| AuditEntry {
        escrow_id: e.id.clone(),
        actor: "test".into(),
        action: "race".into(),
        from_state: EscrowState::InvoiceIssued,
        to_state: to,
        reason: String::new(),
        created_at: T0 + 1,
    };
    let (a, b) = tokio::join!(
        store.transition(&e.id, EscrowState::InvoiceIssued, EscrowUpdate::default(), audit(EscrowState::Locked)),
        store.transition(&e.id, EscrowState::InvoiceIssued, EscrowUpdate::default(), audit(EscrowState::ExpiredUnpaid)),
    );
    let outcomes = [a.map(|e| e.state), b.map(|e| e.state)];
    assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1, "{outcomes:?}");
    assert!(outcomes.iter().any(|r| *r == Err(StoreError::Conflict)), "{outcomes:?}");
    // And only the winner left an audit entry.
    assert_eq!(store.audit_log(&e.id).await.unwrap().len(), 2, "accept + one winner");
}

#[tokio::test]
#[ignore = "needs PostgreSQL"]
async fn a_duplicate_payment_hash_is_a_conflict() {
    let store = Arc::new(fresh().await);
    let svc = EscrowService::new(MockLightning::new(), store, TimingConfig::default());
    let offer = || NewEscrow {
        buyer_id: Some("buyer".into()),
        arbiter: None,
        fiat: None,
        reference: None,
        return_url: None,
        seller_id: "seller".into(),
        amount_sats: 1000,
        description: "dup".into(),
        deadline_at: T0 + 3600,
    };
    let (a, b) = (svc.create(offer(), T0).await.unwrap(), svc.create(offer(), T0).await.unwrap());
    let h = Preimage::from_bytes([4; 32]).hash();
    svc.accept(&a.id, h, "buyer", T0).await.unwrap();
    assert!(svc.accept(&b.id, h, "buyer", T0).await.is_err());
}

#[tokio::test]
#[ignore = "needs PostgreSQL"]
async fn postgres_store_meets_the_payout_store_contract() {
    escrow_core::testing::payout_store_contract(&fresh().await).await;
}

#[tokio::test]
#[ignore = "needs PostgreSQL"]
async fn concurrent_withdrawals_cannot_spend_the_same_sats_twice() {
    use escrow_core::{
        model::{NodeKind, Withdrawal, WithdrawalKind, WithdrawalState},
        store::PayoutStore,
    };
    let store = Arc::new(fresh().await);
    store
        .insert(Escrow {
            id: "earned".into(),
            buyer_id: Some("buyer".into()),
            seller_id: "seller".into(),
            arbiter_id: None,
            arbiter_key: None,
            arbiter_sealed: None,
            arbiter_confirmed_at: None,
            node: NodeKind::Shared,
            amount_sats: 5_000,
            fiat: None,
            reference: None,
            return_url: None,
            description: "race".into(),
            payment_hash: None,
            invoice: None,
            invoice_expires_at: None,
            state: EscrowState::Settled,
            deadline_at: T0 + 3600,
            created_at: T0,
            updated_at: T0,
        })
        .await
        .unwrap();

    // Ten withdrawals of 1,000 (+10 fee allowance) at once against 5,000: exactly four fit.
    let tasks: Vec<_> = (0..10u8)
        .map(|i| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .insert_withdrawal(Withdrawal {
                        id: format!("w{i}"),
                        user_id: "seller".into(),
                        kind: WithdrawalKind::Lightning,
                        amount_sats: 1_000,
                        fee_limit_sats: 10,
                        fee_sats: None,
                        invoice: format!("lnbcrt{i}"),
                        payment_hash: [i; 32],
                        state: WithdrawalState::Pending,
                        failure: None,
                        created_at: T0,
                        updated_at: T0,
                    }, &WithdrawalLimits::NONE)
                    .await
                    .unwrap()
            })
        })
        .collect();
    let mut accepted = 0;
    for t in tasks {
        accepted += usize::from(t.await.unwrap());
    }
    assert_eq!(accepted, 4);
    assert_eq!(store.ledger_totals("seller", &WithdrawalLimits::NONE).await.unwrap().available(), 5_000 - 4 * 1_010);
}
