//! PostgreSQL-backed [`Store`] and [`UserStore`]. Same rules as the SQLite store, and it passes the
//! same contract tests: transitions are compare-and-set on the state inside one transaction, the
//! payment hash is unique, and there is deliberately no column for a preimage.

use async_trait::async_trait;
use escrow_core::{
    fiat::FiatQuote,
    model::{
        ApiKey, AuditEntry, AuditRow, BankAccount, DepositInstructions, Escrow, NodeConnection, NodeEventRecord, NodeInvoiceState,
        NodeKind, PaymentHash, RampOrder, RampState, Scope, User, Webhook, WebhookProgress, Withdrawal, WithdrawalState,
    },
    state::EscrowState,
    store::{
        EscrowUpdate, LedgerTotals, PayoutStore, RampUpdate, Store, StoreError, UserStore, UserTotals, WithdrawalLimits,
        WithdrawalPin,
    },
};
use sqlx::{
    postgres::{PgPoolOptions, PgRow},
    PgPool, Row,
};

/// Schema migrations, applied in order and recorded in `schema_migrations`. Never edit one that
/// has shipped: add a new one.
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/001_initial.sql"),
    include_str!("migrations/002_own_nodes.sql"),
    include_str!("migrations/003_arbitration.sql"),
    include_str!("migrations/004_fiat.sql"),
    include_str!("migrations/005_api_keys.sql"),
    include_str!("migrations/006_checkout.sql"),
    include_str!("migrations/007_webhooks.sql"),
    include_str!("migrations/008_payouts.sql"),
    include_str!("migrations/009_withdrawal_invoice_once.sql"),
    include_str!("migrations/010_withdrawal_pins.sql"),
];

const ESCROW_COLUMNS: &str = "id, buyer_id, seller_id, arbiter_id, arbiter_key, arbiter_sealed, \
     arbiter_confirmed_at, node, amount_sats, \
     fiat_currency, fiat_amount_minor, fiat_rate_minor, fiat_quoted_at, fiat_source, description, payment_hash, \
     invoice, invoice_expires_at, state, deadline_at, created_at, updated_at, reference, return_url";

/// Arbitrary constant naming the advisory lock that serialises migrations between instances.
const MIGRATION_LOCK: i64 = 0x7405_4157;

pub struct PostgresStore {
    pool: PgPool,
}

fn backend(e: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(e.to_string())
}

impl PostgresStore {
    /// Connect to `url` (for example `postgres://user:pass@host/db`) and bring the schema up to date.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new().max_connections(8).connect(url).await.map_err(backend)?;
        migrate(&pool).await?;
        Ok(Self { pool })
    }

    /// Delete everything, for tests that share a database. Never call this on real data.
    pub async fn wipe_for_tests(&self) -> Result<(), StoreError> {
        sqlx::query("TRUNCATE withdrawal_pins, ramp_orders, withdrawals, webhooks, api_keys, arbiter_keys, node_connections, node_events, audit_log, escrows, users RESTART IDENTITY CASCADE")
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }
}

async fn migrate(pool: &PgPool) -> Result<(), StoreError> {
    // One connection holds the lock for the whole run, so two servers starting together cannot both migrate.
    let mut conn = pool.acquire().await.map_err(backend)?;
    sqlx::query("SELECT pg_advisory_lock($1)").bind(MIGRATION_LOCK).execute(&mut *conn).await.map_err(backend)?;
    let result = run_migrations(&mut conn).await;
    sqlx::query("SELECT pg_advisory_unlock($1)").bind(MIGRATION_LOCK).execute(&mut *conn).await.map_err(backend)?;
    result
}

async fn run_migrations(conn: &mut sqlx::PgConnection) -> Result<(), StoreError> {
    sqlx::query("CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY)")
        .execute(&mut *conn)
        .await
        .map_err(backend)?;
    let applied: Option<i32> =
        sqlx::query_scalar("SELECT MAX(version) FROM schema_migrations").fetch_one(&mut *conn).await.map_err(backend)?;
    let applied = applied.unwrap_or(0) as usize;
    if applied > MIGRATIONS.len() {
        return Err(StoreError::Backend(format!(
            "database schema version {applied} is newer than this build understands ({})",
            MIGRATIONS.len()
        )));
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(applied) {
        let mut tx = sqlx::Connection::begin(&mut *conn).await.map_err(backend)?;
        sqlx::raw_sql(sql).execute(&mut *tx).await.map_err(backend)?;
        sqlx::query("INSERT INTO schema_migrations (version) VALUES ($1)")
            .bind(i as i32 + 1)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
    }
    Ok(())
}

fn hash_from_hex(s: &str) -> Result<PaymentHash, StoreError> {
    hex::decode(s)
        .map_err(backend)?
        .try_into()
        .map_err(|_| StoreError::Backend("stored payment hash is not 32 bytes".into()))
}

fn escrow_from_row(row: &PgRow) -> Result<Escrow, StoreError> {
    let payment_hash: Option<String> = row.try_get("payment_hash").map_err(backend)?;
    let state: String = row.try_get("state").map_err(backend)?;
    let node: String = row.try_get("node").map_err(backend)?;
    Ok(Escrow {
        id: row.try_get("id").map_err(backend)?,
        buyer_id: row.try_get("buyer_id").map_err(backend)?,
        seller_id: row.try_get("seller_id").map_err(backend)?,
        arbiter_id: row.try_get("arbiter_id").map_err(backend)?,
        arbiter_key: row.try_get("arbiter_key").map_err(backend)?,
        arbiter_sealed: row.try_get("arbiter_sealed").map_err(backend)?,
        arbiter_confirmed_at: row.try_get("arbiter_confirmed_at").map_err(backend)?,
        node: node.parse::<NodeKind>().map_err(StoreError::Backend)?,
        amount_sats: row.try_get::<i64, _>("amount_sats").map_err(backend)? as u64,
        fiat: fiat_from_row(row)?,
        description: row.try_get("description").map_err(backend)?,
        payment_hash: payment_hash.as_deref().map(hash_from_hex).transpose()?,
        invoice: row.try_get("invoice").map_err(backend)?,
        invoice_expires_at: row.try_get("invoice_expires_at").map_err(backend)?,
        state: state.parse().map_err(StoreError::Backend)?,
        deadline_at: row.try_get("deadline_at").map_err(backend)?,
        created_at: row.try_get("created_at").map_err(backend)?,
        updated_at: row.try_get("updated_at").map_err(backend)?,
        reference: row.try_get("reference").map_err(backend)?,
        return_url: row.try_get("return_url").map_err(backend)?,
    })
}

fn fiat_from_row(row: &PgRow) -> Result<Option<FiatQuote>, StoreError> {
    let currency: Option<String> = row.try_get("fiat_currency").map_err(backend)?;
    let Some(currency) = currency else { return Ok(None) };
    let need = |name: &str| -> Result<i64, StoreError> {
        row.try_get::<Option<i64>, _>(name)
            .map_err(backend)?
            .ok_or_else(|| StoreError::Backend(format!("a stored fiat quote is missing {name}")))
    };
    Ok(Some(FiatQuote {
        currency,
        amount_minor: need("fiat_amount_minor")?,
        rate_minor_per_btc: need("fiat_rate_minor")?,
        quoted_at: need("fiat_quoted_at")?,
        source: row.try_get::<Option<String>, _>("fiat_source").map_err(backend)?.unwrap_or_default(),
    }))
}

fn audit_from_row(row: &PgRow) -> Result<AuditEntry, StoreError> {
    let from: String = row.try_get("from_state").map_err(backend)?;
    let to: String = row.try_get("to_state").map_err(backend)?;
    Ok(AuditEntry {
        escrow_id: row.try_get("escrow_id").map_err(backend)?,
        actor: row.try_get("actor").map_err(backend)?,
        action: row.try_get("action").map_err(backend)?,
        from_state: from.parse().map_err(StoreError::Backend)?,
        to_state: to.parse().map_err(StoreError::Backend)?,
        reason: row.try_get("reason").map_err(backend)?,
        created_at: row.try_get("created_at").map_err(backend)?,
    })
}

fn node_state_from_str(s: &str) -> Result<NodeInvoiceState, StoreError> {
    [
        NodeInvoiceState::Open,
        NodeInvoiceState::Accepted,
        NodeInvoiceState::Settled,
        NodeInvoiceState::Canceled,
    ]
    .into_iter()
    .find(|st| st.as_str() == s)
    .ok_or_else(|| StoreError::Backend(format!("unknown node invoice state '{s}'")))
}

#[async_trait]
impl Store for PostgresStore {
    async fn insert(&self, e: Escrow) -> Result<(), StoreError> {
        let res = sqlx::query(&format!(
            "INSERT INTO escrows ({ESCROW_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24)"
        ))
        .bind(&e.id)
        .bind(&e.buyer_id)
        .bind(&e.seller_id)
        .bind(&e.arbiter_id)
        .bind(&e.arbiter_key)
        .bind(&e.arbiter_sealed)
        .bind(e.arbiter_confirmed_at)
        .bind(e.node.as_str())
        .bind(e.amount_sats as i64)
        .bind(e.fiat.as_ref().map(|f| f.currency.as_str()))
        .bind(e.fiat.as_ref().map(|f| f.amount_minor))
        .bind(e.fiat.as_ref().map(|f| f.rate_minor_per_btc))
        .bind(e.fiat.as_ref().map(|f| f.quoted_at))
        .bind(e.fiat.as_ref().map(|f| f.source.as_str()))
        .bind(&e.description)
        .bind(e.payment_hash.map(hex::encode))
        .bind(&e.invoice)
        .bind(e.invoice_expires_at)
        .bind(e.state.as_str())
        .bind(e.deadline_at)
        .bind(e.created_at)
        .bind(e.updated_at)
        .bind(&e.reference)
        .bind(&e.return_url)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => Err(StoreError::Conflict),
            Err(e) => Err(backend(e)),
        }
    }

    async fn get(&self, id: &str) -> Result<Option<Escrow>, StoreError> {
        let row = sqlx::query(&format!("SELECT {ESCROW_COLUMNS} FROM escrows WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(escrow_from_row).transpose()
    }

    async fn get_by_hash(&self, hash: &PaymentHash) -> Result<Option<Escrow>, StoreError> {
        let row = sqlx::query(&format!("SELECT {ESCROW_COLUMNS} FROM escrows WHERE payment_hash = $1"))
            .bind(hex::encode(hash))
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(escrow_from_row).transpose()
    }

    async fn list_active(&self) -> Result<Vec<Escrow>, StoreError> {
        let terminal: Vec<String> = EscrowState::ALL
            .into_iter()
            .filter(|s| s.is_terminal())
            .map(|s| s.as_str().to_string())
            .collect();
        let rows = sqlx::query(&format!(
            "SELECT {ESCROW_COLUMNS} FROM escrows WHERE NOT (state = ANY($1)) ORDER BY created_at, id"
        ))
        .bind(terminal)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(escrow_from_row).collect()
    }

    async fn list_for_user(&self, user_id: &str) -> Result<Vec<Escrow>, StoreError> {
        // Everything still in play, plus the newest 100 that have ended.
        let ended = ended_states();
        let party = "(buyer_id = $1 OR seller_id = $1 OR arbiter_id = $1)";
        let rows = sqlx::query(&format!(
            "SELECT {ESCROW_COLUMNS} FROM escrows WHERE {party} AND (state NOT IN ({ended}) OR id IN ( \
                 SELECT id FROM escrows WHERE {party} AND state IN ({ended}) \
                 ORDER BY created_at DESC, id ASC LIMIT 100)) \
             ORDER BY created_at DESC, id ASC"
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(escrow_from_row).collect()
    }

    async fn totals_for_user(&self, user_id: &str) -> Result<UserTotals, StoreError> {
        let row = sqlx::query(
            "SELECT COUNT(*)::BIGINT AS escrows, \
                    COALESCE(SUM(CASE WHEN state = 'SETTLED' THEN 1 ELSE 0 END), 0)::BIGINT AS settled_count, \
                    COALESCE(SUM(CASE WHEN state = 'SETTLED' AND seller_id = $1 THEN amount_sats ELSE 0 END), 0)::BIGINT AS settled_received, \
                    COALESCE(SUM(CASE WHEN state = 'SETTLED' AND buyer_id = $1 THEN amount_sats ELSE 0 END), 0)::BIGINT AS settled_paid, \
                    COALESCE(SUM(CASE WHEN state = 'REFUNDED' THEN 1 ELSE 0 END), 0)::BIGINT AS refunded_count, \
                    COALESCE(SUM(CASE WHEN state = 'REFUNDED' AND (buyer_id = $1 OR seller_id = $1) THEN amount_sats ELSE 0 END), 0)::BIGINT AS refunded_sats \
             FROM escrows WHERE buyer_id = $1 OR seller_id = $1 OR arbiter_id = $1",
        )
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(backend)?;
        let n = |col: &str| -> Result<u64, StoreError> {
            Ok(row.try_get::<i64, _>(col).map_err(backend)?.max(0) as u64)
        };
        Ok(UserTotals {
            escrows: n("escrows")?,
            settled_count: n("settled_count")?,
            settled_received: n("settled_received")?,
            settled_paid: n("settled_paid")?,
            refunded_count: n("refunded_count")?,
            refunded_sats: n("refunded_sats")?,
        })
    }

    async fn transition(
        &self,
        id: &str,
        expected: EscrowState,
        update: EscrowUpdate,
        audit: AuditEntry,
    ) -> Result<Escrow, StoreError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;

        // Compare-and-set on the state: only one concurrent actor can move the escrow. Under READ
        // COMMITTED a second writer waits for the first, re-checks `state = $7`, and updates nothing.
        let res = sqlx::query(
            "UPDATE escrows SET state = $1, updated_at = $2, \
                 buyer_id = COALESCE($3, buyer_id), \
                 payment_hash = COALESCE($4, payment_hash), \
                 invoice = COALESCE($5, invoice), \
                 invoice_expires_at = COALESCE($6, invoice_expires_at), \
                 arbiter_sealed = COALESCE($9, arbiter_sealed), \
                 arbiter_confirmed_at = COALESCE($10, arbiter_confirmed_at) \
             WHERE id = $8 AND state = $7",
        )
        .bind(audit.to_state.as_str())
        .bind(audit.created_at)
        .bind(&update.buyer_id)
        .bind(update.payment_hash.map(hex::encode))
        .bind(&update.invoice)
        .bind(update.invoice_expires_at)
        .bind(expected.as_str())
        .bind(id)
        .bind(&update.arbiter_sealed)
        .bind(update.arbiter_confirmed_at)
        .execute(&mut *tx)
        .await;
        let rows = match res {
            Ok(r) => r.rows_affected(),
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => return Err(StoreError::Conflict),
            Err(e) => return Err(backend(e)),
        };
        if rows == 0 {
            let exists = sqlx::query("SELECT 1 FROM escrows WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(backend)?
                .is_some();
            return Err(if exists { StoreError::Conflict } else { StoreError::NotFound });
        }

        sqlx::query(
            "INSERT INTO audit_log (escrow_id, actor, action, from_state, to_state, reason, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&audit.escrow_id)
        .bind(&audit.actor)
        .bind(&audit.action)
        .bind(audit.from_state.as_str())
        .bind(audit.to_state.as_str())
        .bind(&audit.reason)
        .bind(audit.created_at)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;

        let row = sqlx::query(&format!("SELECT {ESCROW_COLUMNS} FROM escrows WHERE id = $1"))
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?;
        let escrow = escrow_from_row(&row)?;
        tx.commit().await.map_err(backend)?;
        Ok(escrow)
    }

    async fn audit_log(&self, id: &str) -> Result<Vec<AuditEntry>, StoreError> {
        let rows = sqlx::query(
            "SELECT escrow_id, actor, action, from_state, to_state, reason, created_at \
             FROM audit_log WHERE escrow_id = $1 ORDER BY seq",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(audit_from_row).collect()
    }

    async fn audit_since(&self, seller_id: &str, after: i64, limit: u32) -> Result<Vec<AuditRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT a.seq, a.escrow_id, a.actor, a.action, a.from_state, a.to_state, a.reason, a.created_at \
             FROM audit_log a JOIN escrows e ON e.id = a.escrow_id \
             WHERE e.seller_id = $1 AND a.seq > $2 ORDER BY a.seq LIMIT $3",
        )
        .bind(seller_id)
        .bind(after)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter()
            .map(|r| Ok(AuditRow { seq: r.try_get("seq").map_err(backend)?, entry: audit_from_row(r)? }))
            .collect()
    }

    async fn audit_head(&self) -> Result<i64, StoreError> {
        sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM audit_log")
            .fetch_one(&self.pool)
            .await
            .map_err(backend)
    }

    async fn record_node_event(&self, ev: NodeEventRecord) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO node_events (escrow_id, node_invoice_state, raw_event_hash, received_at) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&ev.escrow_id)
        .bind(ev.node_invoice_state.as_str())
        .bind(&ev.raw_event_hash)
        .bind(ev.received_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn node_events(&self, id: &str) -> Result<Vec<NodeEventRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT escrow_id, node_invoice_state, raw_event_hash, received_at \
             FROM node_events WHERE escrow_id = $1 ORDER BY seq",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter()
            .map(|r| {
                let state: String = r.try_get("node_invoice_state").map_err(backend)?;
                Ok(NodeEventRecord {
                    escrow_id: r.try_get("escrow_id").map_err(backend)?,
                    node_invoice_state: node_state_from_str(&state)?,
                    raw_event_hash: r.try_get("raw_event_hash").map_err(backend)?,
                    received_at: r.try_get("received_at").map_err(backend)?,
                })
            })
            .collect()
    }
}

fn node_connection_from_row(row: &PgRow) -> Result<NodeConnection, StoreError> {
    Ok(NodeConnection {
        user_id: row.try_get("user_id").map_err(backend)?,
        address: row.try_get("address").map_err(backend)?,
        tls_cert: row.try_get("tls_cert").map_err(backend)?,
        macaroon_enc: row.try_get("macaroon_enc").map_err(backend)?,
        updated_at: row.try_get("updated_at").map_err(backend)?,
    })
}

fn user_from_row(row: &PgRow) -> Result<User, StoreError> {
    let role: String = row.try_get("role").map_err(backend)?;
    Ok(User {
        id: row.try_get("id").map_err(backend)?,
        role: role.parse().map_err(StoreError::Backend)?,
        created_at: row.try_get("created_at").map_err(backend)?,
    })
}

#[async_trait]
impl UserStore for PostgresStore {
    async fn insert_user(&self, user: User, token_hash: String) -> Result<(), StoreError> {
        let res = sqlx::query("INSERT INTO users (id, role, token_hash, created_at) VALUES ($1, $2, $3, $4)")
            .bind(&user.id)
            .bind(user.role.as_str())
            .bind(token_hash)
            .bind(user.created_at)
            .execute(&self.pool)
            .await;
        match res {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => Err(StoreError::Conflict),
            Err(e) => Err(backend(e)),
        }
    }

    async fn user_by_token_hash(&self, token_hash: &str) -> Result<Option<User>, StoreError> {
        let row = sqlx::query("SELECT id, role, created_at FROM users WHERE token_hash = $1")
            .bind(token_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(user_from_row).transpose()
    }

    async fn get_user(&self, id: &str) -> Result<Option<User>, StoreError> {
        let row = sqlx::query("SELECT id, role, created_at FROM users WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(user_from_row).transpose()
    }

    async fn put_arbiter_key(&self, user_id: &str, public_key: &str, at: i64) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO arbiter_keys (user_id, public_key, updated_at) VALUES ($1, $2, $3) \
             ON CONFLICT (user_id) DO UPDATE SET public_key = EXCLUDED.public_key, updated_at = EXCLUDED.updated_at",
        )
        .bind(user_id)
        .bind(public_key)
        .bind(at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn arbiter_key(&self, user_id: &str) -> Result<Option<String>, StoreError> {
        let row = sqlx::query("SELECT public_key FROM arbiter_keys WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.map(|r| r.try_get("public_key").map_err(backend)).transpose()
    }

    async fn put_node_connection(&self, c: NodeConnection) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO node_connections (user_id, address, tls_cert, macaroon_enc, updated_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (user_id) DO UPDATE SET address = EXCLUDED.address, tls_cert = EXCLUDED.tls_cert, \
                 macaroon_enc = EXCLUDED.macaroon_enc, updated_at = EXCLUDED.updated_at",
        )
        .bind(&c.user_id)
        .bind(&c.address)
        .bind(&c.tls_cert)
        .bind(&c.macaroon_enc)
        .bind(c.updated_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn node_connection(&self, user_id: &str) -> Result<Option<NodeConnection>, StoreError> {
        let row = sqlx::query(
            "SELECT user_id, address, tls_cert, macaroon_enc, updated_at FROM node_connections WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.as_ref().map(node_connection_from_row).transpose()
    }

    async fn delete_node_connection(&self, user_id: &str) -> Result<bool, StoreError> {
        let res = sqlx::query("DELETE FROM node_connections WHERE user_id = $1")
            .bind(user_id)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(res.rows_affected() > 0)
    }

    async fn insert_api_key(&self, key: ApiKey, key_hash: String) -> Result<(), StoreError> {
        let res = sqlx::query(
            "INSERT INTO api_keys (id, user_id, name, key_hash, last_chars, scopes, created_at, last_used_at, revoked_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&key.id)
        .bind(&key.user_id)
        .bind(&key.name)
        .bind(&key_hash)
        .bind(&key.last_chars)
        .bind(scopes_to_text(&key.scopes))
        .bind(key.created_at)
        .bind(key.last_used_at)
        .bind(key.revoked_at)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(StoreError::Conflict),
            Err(e) => Err(backend(e)),
        }
    }

    async fn api_key_by_hash(&self, key_hash: &str) -> Result<Option<ApiKey>, StoreError> {
        let row = sqlx::query(&format!("SELECT {API_KEY_COLUMNS} FROM api_keys WHERE key_hash = $1"))
            .bind(key_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(api_key_from_row).transpose()
    }

    async fn list_api_keys(&self, user_id: &str) -> Result<Vec<ApiKey>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {API_KEY_COLUMNS} FROM api_keys WHERE user_id = $1 ORDER BY created_at DESC, id ASC"
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(api_key_from_row).collect()
    }

    async fn revoke_api_key(&self, user_id: &str, id: &str, at: i64) -> Result<bool, StoreError> {
        let res = sqlx::query(
            "UPDATE api_keys SET revoked_at = $1 WHERE id = $2 AND user_id = $3 AND revoked_at IS NULL",
        )
        .bind(at)
        .bind(id)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(res.rows_affected() > 0)
    }

    async fn touch_api_key(&self, id: &str, at: i64) -> Result<(), StoreError> {
        sqlx::query("UPDATE api_keys SET last_used_at = $1 WHERE id = $2")
            .bind(at)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn insert_webhook(&self, h: Webhook) -> Result<(), StoreError> {
        let res = sqlx::query(
            "INSERT INTO webhooks (id, user_id, url, signing_key_enc, created_at, last_seq, attempts, next_attempt_at, \
                                   failing_since, disabled_at, last_attempt_at, last_status) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(&h.id)
        .bind(&h.user_id)
        .bind(&h.url)
        .bind(&h.signing_key_enc)
        .bind(h.created_at)
        .bind(h.last_seq)
        .bind(h.attempts as i64)
        .bind(h.next_attempt_at)
        .bind(h.failing_since)
        .bind(h.disabled_at)
        .bind(h.last_attempt_at)
        .bind(&h.last_status)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(StoreError::Conflict),
            Err(e) => Err(backend(e)),
        }
    }

    async fn get_webhook(&self, id: &str) -> Result<Option<Webhook>, StoreError> {
        let row = sqlx::query(&format!("SELECT {WEBHOOK_COLUMNS} FROM webhooks WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(webhook_from_row).transpose()
    }

    async fn list_webhooks(&self, user_id: &str) -> Result<Vec<Webhook>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {WEBHOOK_COLUMNS} FROM webhooks WHERE user_id = $1 ORDER BY created_at, id"
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(webhook_from_row).collect()
    }

    async fn delete_webhook(&self, user_id: &str, id: &str) -> Result<bool, StoreError> {
        let res = sqlx::query("DELETE FROM webhooks WHERE id = $1 AND user_id = $2")
            .bind(id)
            .bind(user_id)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(res.rows_affected() > 0)
    }

    async fn due_webhooks(&self, now: i64, limit: u32) -> Result<Vec<Webhook>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {WEBHOOK_COLUMNS} FROM webhooks WHERE disabled_at IS NULL AND next_attempt_at <= $1 \
             ORDER BY next_attempt_at, id LIMIT $2"
        ))
        .bind(now)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(webhook_from_row).collect()
    }

    async fn record_webhook_progress(&self, id: &str, p: WebhookProgress) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE webhooks SET last_seq = $1, attempts = $2, next_attempt_at = $3, failing_since = $4, \
                                 disabled_at = $5, last_attempt_at = $6, last_status = $7 WHERE id = $8",
        )
        .bind(p.last_seq)
        .bind(p.attempts as i64)
        .bind(p.next_attempt_at)
        .bind(p.failing_since)
        .bind(p.disabled_at)
        .bind(p.last_attempt_at)
        .bind(&p.last_status)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }
}

/// The terminal states as a SQL list such as `'SETTLED', 'REFUNDED'`. They are fixed names, never input.
fn ended_states() -> String {
    EscrowState::ALL
        .into_iter()
        .filter(|s| s.is_terminal())
        .map(|s| format!("'{}'", s.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

const API_KEY_COLUMNS: &str = "id, user_id, name, last_chars, scopes, created_at, last_used_at, revoked_at";

fn scopes_to_text(scopes: &[Scope]) -> String {
    scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(",")
}

fn api_key_from_row(row: &PgRow) -> Result<ApiKey, StoreError> {
    let scopes: String = row.try_get("scopes").map_err(backend)?;
    Ok(ApiKey {
        id: row.try_get("id").map_err(backend)?,
        user_id: row.try_get("user_id").map_err(backend)?,
        name: row.try_get("name").map_err(backend)?,
        last_chars: row.try_get("last_chars").map_err(backend)?,
        scopes: scopes.split(',').filter(|s| !s.is_empty()).map(|s| s.parse().map_err(StoreError::Backend)).collect::<Result<_, _>>()?,
        created_at: row.try_get("created_at").map_err(backend)?,
        last_used_at: row.try_get("last_used_at").map_err(backend)?,
        revoked_at: row.try_get("revoked_at").map_err(backend)?,
    })
}

const WEBHOOK_COLUMNS: &str = "id, user_id, url, signing_key_enc, created_at, last_seq, attempts, next_attempt_at, \
     failing_since, disabled_at, last_attempt_at, last_status";

fn webhook_from_row(row: &PgRow) -> Result<Webhook, StoreError> {
    Ok(Webhook {
        id: row.try_get("id").map_err(backend)?,
        user_id: row.try_get("user_id").map_err(backend)?,
        url: row.try_get("url").map_err(backend)?,
        signing_key_enc: row.try_get("signing_key_enc").map_err(backend)?,
        created_at: row.try_get("created_at").map_err(backend)?,
        last_seq: row.try_get("last_seq").map_err(backend)?,
        attempts: row.try_get::<i64, _>("attempts").map_err(backend)?.max(0) as u32,
        next_attempt_at: row.try_get("next_attempt_at").map_err(backend)?,
        failing_since: row.try_get("failing_since").map_err(backend)?,
        disabled_at: row.try_get("disabled_at").map_err(backend)?,
        last_attempt_at: row.try_get("last_attempt_at").map_err(backend)?,
        last_status: row.try_get("last_status").map_err(backend)?,
    })
}

const WITHDRAWAL_COLUMNS: &str = "id, user_id, kind, amount_sats, fee_limit_sats, fee_sats, invoice, payment_hash, state, \
     failure, created_at, updated_at";
const RAMP_COLUMNS: &str = "id, user_id, direction, escrow_id, withdrawal_id, amount_sats, currency, fiat_minor, fee_minor, \
     rate_minor_per_btc, bank_code, bank_name, account_number, account_name, deposit_bank_name, deposit_account_number, \
     deposit_account_name, deposit_reference, deposit_expires_at, provider, fiat_reference, state, note, created_at, updated_at";
/// Names the advisory locks that serialise one user's withdrawals (the second key is the user).
const WITHDRAWAL_LOCK: i32 = 0x7405;
/// Ramp states after which an order never moves again; kept in step with `RampState::is_terminal`.
const RAMP_TERMINAL: &str = "('complete', 'refunded', 'expired', 'paid_out', 'failed')";

fn parse<T: std::str::FromStr<Err = String>>(row: &PgRow, col: &str) -> Result<T, StoreError> {
    row.try_get::<String, _>(col).map_err(backend)?.parse().map_err(StoreError::Backend)
}

fn withdrawal_from_row(row: &PgRow) -> Result<Withdrawal, StoreError> {
    Ok(Withdrawal {
        id: row.try_get("id").map_err(backend)?,
        user_id: row.try_get("user_id").map_err(backend)?,
        kind: parse(row, "kind")?,
        amount_sats: row.try_get::<i64, _>("amount_sats").map_err(backend)? as u64,
        fee_limit_sats: row.try_get::<i64, _>("fee_limit_sats").map_err(backend)? as u64,
        fee_sats: row.try_get::<Option<i64>, _>("fee_sats").map_err(backend)?.map(|f| f as u64),
        invoice: row.try_get("invoice").map_err(backend)?,
        payment_hash: hash_from_hex(&row.try_get::<String, _>("payment_hash").map_err(backend)?)?,
        state: parse(row, "state")?,
        failure: row.try_get("failure").map_err(backend)?,
        created_at: row.try_get("created_at").map_err(backend)?,
        updated_at: row.try_get("updated_at").map_err(backend)?,
    })
}

fn ramp_from_row(row: &PgRow) -> Result<RampOrder, StoreError> {
    let deposit_reference: Option<String> = row.try_get("deposit_reference").map_err(backend)?;
    let deposit = match deposit_reference {
        Some(reference) => Some(DepositInstructions {
            bank_name: row.try_get::<Option<String>, _>("deposit_bank_name").map_err(backend)?.unwrap_or_default(),
            account_number: row.try_get::<Option<String>, _>("deposit_account_number").map_err(backend)?.unwrap_or_default(),
            account_name: row.try_get::<Option<String>, _>("deposit_account_name").map_err(backend)?.unwrap_or_default(),
            reference,
            expires_at: row.try_get::<Option<i64>, _>("deposit_expires_at").map_err(backend)?.unwrap_or_default(),
        }),
        None => None,
    };
    Ok(RampOrder {
        id: row.try_get("id").map_err(backend)?,
        user_id: row.try_get("user_id").map_err(backend)?,
        direction: parse(row, "direction")?,
        escrow_id: row.try_get("escrow_id").map_err(backend)?,
        withdrawal_id: row.try_get("withdrawal_id").map_err(backend)?,
        amount_sats: row.try_get::<i64, _>("amount_sats").map_err(backend)? as u64,
        currency: row.try_get("currency").map_err(backend)?,
        fiat_minor: row.try_get("fiat_minor").map_err(backend)?,
        fee_minor: row.try_get("fee_minor").map_err(backend)?,
        rate_minor_per_btc: row.try_get("rate_minor_per_btc").map_err(backend)?,
        bank: BankAccount {
            bank_code: row.try_get("bank_code").map_err(backend)?,
            bank_name: row.try_get("bank_name").map_err(backend)?,
            account_number: row.try_get("account_number").map_err(backend)?,
            account_name: row.try_get("account_name").map_err(backend)?,
        },
        deposit,
        provider: row.try_get("provider").map_err(backend)?,
        fiat_reference: row.try_get("fiat_reference").map_err(backend)?,
        state: parse(row, "state")?,
        note: row.try_get("note").map_err(backend)?,
        created_at: row.try_get("created_at").map_err(backend)?,
        updated_at: row.try_get("updated_at").map_err(backend)?,
    })
}

/// The balance arithmetic in SQL, for one user bound twice as `?`: earned from settled shared-node
/// escrows as seller, held by pending withdrawals, and spent by those that went through.
/// Binds: 1 user, 2 matured-before, 3 window-from.
const LEDGER_SQL: &str = "SELECT \
     (SELECT COALESCE(SUM(amount_sats), 0)::BIGINT FROM escrows WHERE seller_id = $1 AND node = 'shared' AND state = 'SETTLED') AS earned, \
     (SELECT COALESCE(SUM(e.amount_sats), 0)::BIGINT FROM escrows e WHERE e.seller_id = $1 AND e.node = 'shared' \
          AND e.state = 'SETTLED' AND e.updated_at > $2 AND EXISTS (SELECT 1 FROM ramp_orders r \
          WHERE r.escrow_id = e.id AND r.state IN ('paying', 'funded', 'complete'))) AS maturing, \
     (SELECT COALESCE(SUM(amount_sats + fee_limit_sats), 0)::BIGINT FROM withdrawals WHERE user_id = $1 AND state = 'pending') AS held, \
     (SELECT COALESCE(SUM(amount_sats + COALESCE(fee_sats, 0)), 0)::BIGINT FROM withdrawals WHERE user_id = $1 AND state = 'succeeded') AS spent, \
     (SELECT COALESCE(SUM(amount_sats), 0)::BIGINT FROM withdrawals WHERE user_id = $1 AND state <> 'failed' AND created_at >= $3) AS in_window";

#[async_trait]
impl PayoutStore for PostgresStore {
    async fn ledger_totals(&self, user_id: &str, limits: &WithdrawalLimits) -> Result<LedgerTotals, StoreError> {
        let row = sqlx::query(LEDGER_SQL)
            .bind(user_id)
            .bind(limits.matured_before)
            .bind(limits.window_from)
            .fetch_one(&self.pool)
            .await
            .map_err(backend)?;
        let get = |c: &str| row.try_get::<i64, _>(c).map(|v| v as u64).map_err(backend);
        Ok(LedgerTotals {
            earned_sats: get("earned")?,
            maturing_sats: get("maturing")?,
            withdrawn_in_window_sats: get("in_window")?,
            held_sats: get("held")?,
            withdrawn_sats: get("spent")?,
        })
    }

    async fn insert_withdrawal(&self, w: Withdrawal, limits: &WithdrawalLimits) -> Result<bool, StoreError> {
        // Under READ COMMITTED two inserts could both see the same balance, so a per-user lock held to
        // the end of the transaction makes the check and the insert one step.
        let mut tx = self.pool.begin().await.map_err(backend)?;
        sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
            .bind(WITHDRAWAL_LOCK)
            .bind(&w.user_id)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        let res = sqlx::query(&format!(
            "INSERT INTO withdrawals ({WITHDRAWAL_COLUMNS}) \
             SELECT $4, $1, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14 \
             FROM ({LEDGER_SQL}) AS t \
             WHERE t.earned - t.maturing - t.held - t.spent >= $15 AND t.in_window + $16 <= $17"
        ))
        .bind(&w.user_id)
        .bind(limits.matured_before)
        .bind(limits.window_from)
        .bind(&w.id)
        .bind(w.kind.as_str())
        .bind(w.amount_sats as i64)
        .bind(w.fee_limit_sats as i64)
        .bind(w.fee_sats.map(|f| f as i64))
        .bind(&w.invoice)
        .bind(hex::encode(w.payment_hash))
        .bind(w.state.as_str())
        .bind(&w.failure)
        .bind(w.created_at)
        .bind(w.updated_at)
        .bind(w.debit() as i64)
        .bind(w.amount_sats as i64)
        .bind(i64::try_from(limits.window_cap_sats).unwrap_or(i64::MAX))
        .execute(&mut *tx)
        .await;
        match res {
            Ok(r) => {
                tx.commit().await.map_err(backend)?;
                Ok(r.rows_affected() == 1)
            }
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(StoreError::Conflict),
            Err(e) => Err(backend(e)),
        }
    }

    async fn get_withdrawal(&self, id: &str) -> Result<Option<Withdrawal>, StoreError> {
        let row = sqlx::query(&format!("SELECT {WITHDRAWAL_COLUMNS} FROM withdrawals WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(withdrawal_from_row).transpose()
    }

    async fn list_withdrawals(&self, user_id: &str) -> Result<Vec<Withdrawal>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {WITHDRAWAL_COLUMNS} FROM withdrawals WHERE user_id = $1 ORDER BY created_at DESC, id LIMIT 100"
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(withdrawal_from_row).collect()
    }

    async fn pending_withdrawals(&self) -> Result<Vec<Withdrawal>, StoreError> {
        let rows = sqlx::query(&format!("SELECT {WITHDRAWAL_COLUMNS} FROM withdrawals WHERE state = 'pending' ORDER BY created_at, id"))
            .fetch_all(&self.pool)
            .await
            .map_err(backend)?;
        rows.iter().map(withdrawal_from_row).collect()
    }

    async fn finish_withdrawal(
        &self,
        id: &str,
        state: WithdrawalState,
        fee_sats: Option<u64>,
        failure: Option<String>,
        at: i64,
    ) -> Result<bool, StoreError> {
        let res = sqlx::query(
            "UPDATE withdrawals SET state = $1, fee_sats = $2, failure = $3, updated_at = $4 WHERE id = $5 AND state = 'pending'",
        )
        .bind(state.as_str())
        .bind(fee_sats.map(|f| f as i64))
        .bind(&failure)
        .bind(at)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(res.rows_affected() == 1)
    }

    async fn insert_ramp_order(&self, o: RampOrder) -> Result<(), StoreError> {
        let d = o.deposit.as_ref();
        let res = sqlx::query(&format!(
            "INSERT INTO ramp_orders ({RAMP_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25)"
        ))
        .bind(&o.id)
        .bind(&o.user_id)
        .bind(o.direction.as_str())
        .bind(&o.escrow_id)
        .bind(&o.withdrawal_id)
        .bind(o.amount_sats as i64)
        .bind(&o.currency)
        .bind(o.fiat_minor)
        .bind(o.fee_minor)
        .bind(o.rate_minor_per_btc)
        .bind(&o.bank.bank_code)
        .bind(&o.bank.bank_name)
        .bind(&o.bank.account_number)
        .bind(&o.bank.account_name)
        .bind(d.map(|d| d.bank_name.clone()))
        .bind(d.map(|d| d.account_number.clone()))
        .bind(d.map(|d| d.account_name.clone()))
        .bind(d.map(|d| d.reference.clone()))
        .bind(d.map(|d| d.expires_at))
        .bind(&o.provider)
        .bind(&o.fiat_reference)
        .bind(o.state.as_str())
        .bind(&o.note)
        .bind(o.created_at)
        .bind(o.updated_at)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(StoreError::Conflict),
            Err(e) => Err(backend(e)),
        }
    }

    async fn get_ramp_order(&self, id: &str) -> Result<Option<RampOrder>, StoreError> {
        let row = sqlx::query(&format!("SELECT {RAMP_COLUMNS} FROM ramp_orders WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(ramp_from_row).transpose()
    }

    async fn list_ramp_orders(&self, user_id: &str) -> Result<Vec<RampOrder>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {RAMP_COLUMNS} FROM ramp_orders WHERE user_id = $1 ORDER BY created_at DESC, id LIMIT 100"
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(ramp_from_row).collect()
    }

    async fn open_ramp_orders(&self) -> Result<Vec<RampOrder>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {RAMP_COLUMNS} FROM ramp_orders WHERE state NOT IN {RAMP_TERMINAL} ORDER BY created_at, id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(ramp_from_row).collect()
    }

    async fn ramp_order_for_escrow(&self, escrow_id: &str) -> Result<Option<RampOrder>, StoreError> {
        let row = sqlx::query(&format!(
            "SELECT {RAMP_COLUMNS} FROM ramp_orders WHERE escrow_id = $1 ORDER BY created_at DESC, id LIMIT 1"
        ))
        .bind(escrow_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.as_ref().map(ramp_from_row).transpose()
    }

    async fn update_ramp_order(
        &self,
        id: &str,
        expected: RampState,
        to: RampState,
        update: RampUpdate,
        at: i64,
    ) -> Result<bool, StoreError> {
        let res = sqlx::query(
            "UPDATE ramp_orders SET state = $1, updated_at = $2, note = COALESCE($3, note), \
                 fiat_reference = COALESCE($4, fiat_reference), bank_code = COALESCE($5, bank_code), \
                 bank_name = COALESCE($6, bank_name), account_number = COALESCE($7, account_number), \
                 account_name = COALESCE($8, account_name) WHERE id = $9 AND state = $10",
        )
        .bind(to.as_str())
        .bind(at)
        .bind(&update.note)
        .bind(&update.fiat_reference)
        .bind(update.refund_to.as_ref().map(|b| &b.bank_code))
        .bind(update.refund_to.as_ref().map(|b| &b.bank_name))
        .bind(update.refund_to.as_ref().map(|b| &b.account_number))
        .bind(update.refund_to.as_ref().map(|b| &b.account_name))
        .bind(id)
        .bind(expected.as_str())
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(res.rows_affected() == 1)
    }

    async fn get_withdrawal_pin(&self, user_id: &str) -> Result<Option<WithdrawalPin>, StoreError> {
        let row = sqlx::query("SELECT pin_hash, failures, locked_until, updated_at FROM withdrawal_pins WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.map(|r| {
            Ok(WithdrawalPin {
                pin_hash: r.try_get("pin_hash").map_err(backend)?,
                failures: r.try_get::<i32, _>("failures").map_err(backend)? as u32,
                locked_until: r.try_get("locked_until").map_err(backend)?,
                updated_at: r.try_get("updated_at").map_err(backend)?,
            })
        })
        .transpose()
    }

    async fn set_withdrawal_pin(&self, user_id: &str, pin_hash: &str, at: i64) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO withdrawal_pins (user_id, pin_hash, failures, locked_until, updated_at) VALUES ($1, $2, 0, NULL, $3) ON CONFLICT (user_id) DO UPDATE SET pin_hash = excluded.pin_hash, failures = 0, locked_until = NULL, updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(pin_hash)
        .bind(at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn withdrawal_pin_failed(&self, user_id: &str, max_failures: u32, lock_until: i64) -> Result<(), StoreError> {
        sqlx::query("UPDATE withdrawal_pins SET failures = failures + 1, locked_until = CASE WHEN failures + 1 >= $1 THEN $2 ELSE locked_until END WHERE user_id = $3")
            .bind(max_failures as i32)
            .bind(lock_until)
            .bind(user_id)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn withdrawal_pin_ok(&self, user_id: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE withdrawal_pins SET failures = 0, locked_until = NULL WHERE user_id = $1").bind(user_id).execute(&self.pool).await.map_err(backend)?;
        Ok(())
    }
}
