use std::sync::Arc;

use escrow_core::{
    store::WithdrawalLimits,
    mock::MockLightning,
    model::{NodeInvoiceState, Preimage},
    service::{EscrowService, NewEscrow},
    state::EscrowState,
    store::Store,
    testing::store_contract,
    timing::TimingConfig,
};
use escrow_sqlite::SqliteStore;

const T0: i64 = 1_000_000;

#[tokio::test]
async fn sqlite_store_meets_the_store_contract() {
    store_contract(&SqliteStore::in_memory().await.unwrap()).await;
}

#[tokio::test]
async fn sqlite_store_lists_a_long_history_and_totals_all_of_it() {
    escrow_core::testing::history_contract(&SqliteStore::in_memory().await.unwrap()).await;
}

#[tokio::test]
async fn sqlite_store_meets_the_user_store_contract() {
    escrow_core::testing::user_store_contract(&SqliteStore::in_memory().await.unwrap()).await;
}

fn temp_db(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("escrow-sqlite-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("escrow.db").to_string_lossy().into_owned()
}

#[tokio::test]
async fn full_flow_survives_a_restart_on_a_real_database_file() {
    let path = temp_db("restart");
    let node = MockLightning::new();
    let p = Preimage::from_bytes([5; 32]);

    // First run: create, issue the invoice, then the buyer pays while we are "down".
    let id = {
        let store = Arc::new(SqliteStore::open(&path).await.unwrap());
        let svc = EscrowService::new(node.clone(), store, TimingConfig::default());
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
                    description: "parcel".into(),
                    deadline_at: T0 + 3600,
                },
                T0,
            )
            .await
            .unwrap();
        svc.accept(&e.id, p.hash(), "buyer", T0).await.unwrap();
        e.id
    };
    node.pay(&p.hash());

    // Second run: reopen the same file, reconcile against the node, then settle.
    let store = Arc::new(SqliteStore::open(&path).await.unwrap());
    let svc = EscrowService::new(node.clone(), store.clone(), TimingConfig::default());
    assert_eq!(svc.get(&id).await.unwrap().state, EscrowState::InvoiceIssued);

    let report = svc.reconcile(T0 + 30).await.unwrap();
    assert_eq!(report.changed, vec![id.clone()]);
    assert_eq!(svc.get(&id).await.unwrap().state, EscrowState::Locked);

    let e = svc.release(&id, &p, "seller", T0 + 60).await.unwrap();
    assert_eq!(e.state, EscrowState::Settled);
    assert_eq!(node.state(&p.hash()), Some(NodeInvoiceState::Settled));

    let log = svc.audit_log(&id).await.unwrap();
    assert_eq!(log.len(), 3);
    assert_eq!(log.last().unwrap().to_state, EscrowState::Settled);
    assert!(store.list_active().await.unwrap().is_empty());
}

#[tokio::test]
async fn two_escrows_cannot_share_a_payment_hash_even_around_the_service() {
    let store = SqliteStore::in_memory().await.unwrap();
    let svc = EscrowService::new(MockLightning::new(), Arc::new(store), TimingConfig::default());
    let new = || NewEscrow {
        buyer_id: Some("b".into()),
        arbiter: None,
        fiat: None,
        reference: None,
        return_url: None,
        seller_id: "s".into(),
        amount_sats: 1,
        description: "x".into(),
        deadline_at: T0 + 3600,
    };
    let (a, b) = (svc.create(new(), T0).await.unwrap(), svc.create(new(), T0).await.unwrap());
    let h = Preimage::from_bytes([1; 32]).hash();
    svc.accept(&a.id, h, "b", T0).await.unwrap();
    assert!(svc.accept(&b.id, h, "b", T0).await.is_err());
}

/// No column anywhere can hold the release secret. The one thing that relates to it is
/// `arbiter_sealed`: the secret encrypted to the arbiter's key, which the server cannot read.
#[tokio::test]
async fn the_database_has_no_place_to_keep_a_preimage() {
    use sqlx::{sqlite::SqliteConnectOptions, ConnectOptions, Row};
    let path = temp_db("schema");
    drop(SqliteStore::open(&path).await.unwrap());

    let mut conn = SqliteConnectOptions::new().filename(&path).connect().await.unwrap();
    let tables: Vec<String> = sqlx::query("SELECT name FROM sqlite_master WHERE type = 'table'")
        .fetch_all(&mut conn)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();
    let mut columns = Vec::new();
    for t in &tables {
        for row in sqlx::query(&format!("PRAGMA table_info({t})")).fetch_all(&mut conn).await.unwrap() {
            columns.push(format!("{t}.{}", row.get::<String, _>("name")));
        }
    }
    assert!(columns.len() > 20, "found the schema: {columns:?}");
    for c in &columns {
        let name = c.to_lowercase();
        assert!(!name.contains("preimage"), "{c}");
        // The only column mentioning a secret-like word is the sealed (encrypted) blob.
        assert!(!name.contains("secret") || name.ends_with("sealed"), "{c}");
    }
    assert!(columns.contains(&"escrows.arbiter_sealed".to_string()));
}

/// A database created before migrations existed (schema applied, `user_version` 0, a real escrow
/// with history in it) is upgraded in place: the data survives and open offers become possible.
#[tokio::test]
async fn an_old_database_is_migrated_without_losing_data() {
    use sqlx::{sqlite::SqliteConnectOptions, ConnectOptions, Connection};
    let path = temp_db("migrate");
    let mut conn = SqliteConnectOptions::new().filename(&path).create_if_missing(true).connect().await.unwrap();
    sqlx::raw_sql(include_str!("../src/migrations/001_initial.sql")).execute(&mut conn).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO escrows VALUES ('old', 'b', 's', 1000, 'parcel', NULL, NULL, NULL, 'CREATED', 9000, 1, 1);
         INSERT INTO audit_log (escrow_id, actor, action, from_state, to_state, reason, created_at)
             VALUES ('old', 's', 'create', 'CREATED', 'CREATED', 'kept', 1);",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();

    let store = SqliteStore::open(&path).await.unwrap();
    let old = store.get("old").await.unwrap().expect("the escrow survived");
    assert_eq!((old.buyer_id.as_deref(), old.seller_id.as_str(), old.amount_sats), (Some("b"), "s", 1000));
    assert_eq!(store.audit_log("old").await.unwrap()[0].reason, "kept", "history survived");
    let mut open = old.clone();
    open.id = "open".into();
    open.buyer_id = None;
    store.insert(open).await.expect("buyer_id may now be empty");
    drop(store);

    // Opening again is a no-op, and the version is recorded.
    let store = SqliteStore::open(&path).await.unwrap();
    assert_eq!(store.get("open").await.unwrap().unwrap().buyer_id, None);
    drop(store);
    let mut conn = SqliteConnectOptions::new().filename(&path).connect().await.unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&mut conn).await.unwrap();
    assert_eq!(version, 11);
}

#[tokio::test]
async fn sqlite_store_meets_the_payout_store_contract() {
    escrow_core::testing::payout_store_contract(&SqliteStore::in_memory().await.unwrap()).await;
}

#[tokio::test]
async fn concurrent_withdrawals_on_a_database_file_cannot_spend_the_same_sats_twice() {
    use escrow_core::{
        model::{Escrow, NodeKind, Withdrawal, WithdrawalKind, WithdrawalState},
        store::PayoutStore,
    };
    let store = Arc::new(SqliteStore::open(&temp_db("withdrawal-race")).await.unwrap());
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
            })
        })
        .collect();
    let mut accepted = 0;
    for t in tasks {
        // A writer that found the database busy fails cleanly rather than overspending.
        if let Ok(true) = t.await.unwrap() {
            accepted += 1;
        }
    }
    assert!(accepted <= 4, "{accepted} withdrawals were accepted");
    assert!(store.ledger_totals("seller", &WithdrawalLimits::NONE).await.unwrap().available() >= 5_000 - 4 * 1_010);
}
