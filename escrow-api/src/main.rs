//! toshi-server: the API, the node event loop and the deadline scheduler in one process.
//!
//! Configuration is by environment variable:
//!   OPERATOR_TOKEN   required, token for the operator account
//!   DATABASE_URL     postgres://user:pass@host/db to use PostgreSQL (takes precedence)
//!   DATABASE_PATH    SQLite file when DATABASE_URL is not set (default ./toshi.db)
//!   BIND             listen address (default 127.0.0.1:3000)
//!   SHARED_NODE      "off" to require every seller to bring their own node (default "on")
//!   NODE_CREDENTIAL_KEY  64 hex characters (openssl rand -hex 32); enables sellers' own nodes
//!   FIAT_PROVIDER    off (default), coingecko, mempool, or fixed:USD=65000,NGN=97500000 (demo rates)
//!                    Shows an approximate fiat value next to amounts, taken once at creation. Display only.
//!   RATE_LIMIT_PER_MIN       requests per minute per caller (default 120)
//!   REGISTER_LIMIT_PER_MIN   account registrations per minute per address (default 10; raise it for a demo
//!                            where many people share one network address)
//!   ALLOW_PRIVATE_NODE_ADDRESSES  "1" to let sellers use private network addresses (regtest only)
//!   LND_ADDR         shared node, default https://localhost:10003
//!   LND_TLS_CERT     path to tls.cert
//!   LND_MACAROON     path to a macaroon limited to invoice operations
//!   PAYOUT_MACAROON  path to a macaroon for the shared node that may also pay (offchain:read offchain:write
//!                    invoices:read invoices:write info:read). With it, sellers on the shared node get a balance
//!                    they can withdraw any time. Without it there are no balances. Off by default.
//!   RAMP_PROVIDER    off (default) or demo: pay and get paid in naira. Needs FIAT_PROVIDER to price NGN.
//!                    "demo" moves no real money: its Lightning side is RAMP_LND_ADDR (default
//!                    https://localhost:10001), RAMP_LND_TLS_CERT and RAMP_LND_MACAROON (a payout macaroon).
//!   RAMP_FEE_BPS     the ramp's fee in hundredths of a percent (default 150, which is 1.5%)
//!   NAIRA_HOLD_HOURS sats from an escrow paid in naira can be withdrawn this long after it settles
//!                    (default 24): the time a bank transfer can still be reversed
//!   WITHDRAW_LIMIT_SATS  the most one seller may withdraw in any 24 hours (default 1000000)
//!   CORS_ORIGINS     comma-separated browser origins (default http://localhost:3001,http://127.0.0.1:3001)
//!   PUBLIC_WEB_URL   the web app's public address (for example https://pay.example.com); with it the API
//!                    returns a checkout link for each escrow. Off by default.
//!   RUST_LOG         log filter (default info)

use std::{net::SocketAddr, sync::Arc, time::Duration};

use escrow_api::{ensure_operator, router, ApiConfig, AppState};
use escrow_core::{
    payout::{PayoutLimits, PayoutService, Wallet},
    ramp::{DemoRamp, RampConfig, RampService},
    service::EscrowService,
    store::{PayoutStore, Store, UserStore},
    timing::TimingConfig,
};
use escrow_lnd::{creds::CredentialKey, directory::LndDirectory, runner, LndAdapter};
use escrow_postgres::PostgresStore;
use escrow_sqlite::SqliteStore;

fn env(name: &str, default: Option<&str>) -> String {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => v,
        _ => default.map(str::to_string).unwrap_or_else(|| {
            eprintln!("missing required environment variable {name}");
            std::process::exit(2);
        }),
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let operator_token = env("OPERATOR_TOKEN", None);
    if operator_token.len() < 24 {
        eprintln!("OPERATOR_TOKEN must be at least 24 characters");
        std::process::exit(2);
    }
    let bind: SocketAddr = env("BIND", Some("127.0.0.1:3000")).parse()?;

    // The service's own node, for sellers who have none. Optional: `SHARED_NODE=off` makes every
    // seller bring their own, so the service never receives anyone's money.
    let shared = if env("SHARED_NODE", Some("on")) == "off" {
        tracing::info!("shared node disabled: every seller must connect their own");
        None
    } else {
        Some(
            LndAdapter::connect(
                &env("LND_ADDR", Some("https://localhost:10003")),
                env("LND_TLS_CERT", Some("regtest/creds/seller/tls.cert")),
                env("LND_MACAROON", Some("regtest/creds/seller/toshi.macaroon")),
                Duration::from_secs(10),
            )
            .await?,
        )
    };

    // Paying sellers out needs a macaroon that may send payments; the escrow one may not.
    let payout_wallet: Option<Arc<dyn Wallet>> = match (shared.is_some(), std::env::var("PAYOUT_MACAROON").ok().filter(|m| !m.is_empty())) {
        (true, Some(macaroon)) => {
            let node = LndAdapter::connect(
                &env("LND_ADDR", Some("https://localhost:10003")),
                env("LND_TLS_CERT", Some("regtest/creds/seller/tls.cert")),
                macaroon,
                Duration::from_secs(10),
            )
            .await?;
            tracing::info!("balances on: shared-node sellers can withdraw what they are owed");
            Some(Arc::new(node))
        }
        (false, Some(_)) => {
            tracing::warn!("PAYOUT_MACAROON is set but there is no shared node, so there are no balances to pay out");
            None
        }
        (_, None) => None,
    };
    let ramp_wallet: Option<Arc<dyn Wallet>> = match env("RAMP_PROVIDER", Some("off")).as_str() {
        "off" => None,
        "demo" => {
            let node = LndAdapter::connect(
                &env("RAMP_LND_ADDR", Some("https://localhost:10001")),
                env("RAMP_LND_TLS_CERT", Some("regtest/creds/buyer/tls.cert")),
                env("RAMP_LND_MACAROON", Some("regtest/creds/buyer/payout.macaroon")),
                Duration::from_secs(10),
            )
            .await?;
            // The demo takes "I've sent the transfer" on trust and pays with this node's sats. On a
            // real network that would hand out real money for pretend transfers, so refuse to start.
            let network = node.network().await?;
            if network != "regtest" {
                eprintln!("RAMP_PROVIDER=demo only runs against a regtest node; RAMP_LND_ADDR is on {network}");
                std::process::exit(2);
            }
            tracing::warn!("RAMP_PROVIDER=demo: naira payments are pretend (regtest only)");
            Some(Arc::new(node))
        }
        other => {
            eprintln!("RAMP_PROVIDER must be off or demo, not '{other}'");
            std::process::exit(2);
        }
    };
    let ramp_fee_bps: u32 = env("RAMP_FEE_BPS", Some("150")).parse().unwrap_or_else(|_| {
        eprintln!("RAMP_FEE_BPS must be a whole number");
        std::process::exit(2);
    });
    let number = |name: &str, default: &str| -> u64 {
        env(name, Some(default)).parse().unwrap_or_else(|_| {
            eprintln!("{name} must be a whole number");
            std::process::exit(2);
        })
    };
    let payout_limits = PayoutLimits {
        naira_hold_secs: number("NAIRA_HOLD_HOURS", "24") as i64 * 3600,
        window_cap_sats: number("WITHDRAW_LIMIT_SATS", "1000000"),
        ..PayoutLimits::default()
    };
    let money = Money { payout_wallet, ramp_wallet, ramp_fee_bps, payout_limits };

    // Sellers' macaroons are encrypted at rest under this key; without it sellers cannot connect nodes.
    let key = match std::env::var("NODE_CREDENTIAL_KEY").ok().filter(|k| !k.is_empty()) {
        Some(k) => Some(CredentialKey::from_hex(&k).map_err(|e| format!("NODE_CREDENTIAL_KEY: {e}"))?),
        None => {
            tracing::warn!("NODE_CREDENTIAL_KEY is not set: sellers cannot connect their own nodes (generate one with `openssl rand -hex 32`)");
            None
        }
    };
    let allow_private = env("ALLOW_PRIVATE_NODE_ADDRESSES", Some("0")) == "1";
    if allow_private {
        tracing::warn!("ALLOW_PRIVATE_NODE_ADDRESSES is on: sellers can point the server at private networks (regtest only)");
    }

    // PostgreSQL when DATABASE_URL is set, otherwise a SQLite file. Both pass the same store contract.
    match std::env::var("DATABASE_URL").ok().filter(|u| !u.is_empty()) {
        Some(url) => {
            tracing::info!("using PostgreSQL");
            run(Arc::new(PostgresStore::connect(&url).await?), shared, money, key, allow_private, operator_token, bind).await
        }
        None => {
            let path = env("DATABASE_PATH", Some("toshi.db"));
            tracing::info!(%path, "using SQLite");
            run(Arc::new(SqliteStore::open(&path).await?), shared, money, key, allow_private, operator_token, bind).await
        }
    }
}

/// The nodes that pay sellers out and play the ramp provider's Lightning funds, if configured.
struct Money {
    payout_wallet: Option<Arc<dyn Wallet>>,
    ramp_wallet: Option<Arc<dyn Wallet>>,
    ramp_fee_bps: u32,
    payout_limits: PayoutLimits,
}

async fn run<S>(
    store: S,
    shared: Option<LndAdapter>,
    money: Money,
    key: Option<CredentialKey>,
    allow_private: bool,
    operator_token: String,
    bind: SocketAddr,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: Store + UserStore + PayoutStore + Clone + 'static,
{
    let webhook_key = key.clone();
    let directory = LndDirectory::new(shared, store.clone(), key, Duration::from_secs(10))
        .allow_private_addresses(allow_private);
    let service = Arc::new(EscrowService::new(directory, store.clone(), TimingConfig::default()));
    ensure_operator(&store, &operator_token, runner::unix_now()).await.map_err(|e| e.message)?;

    let limit = |name: &str, default: u32| -> u32 {
        env(name, Some(&default.to_string())).parse().unwrap_or_else(|_| {
            eprintln!("{name} must be a whole number");
            std::process::exit(2);
        })
    };
    let config = ApiConfig {
        rate_limit_per_min: limit("RATE_LIMIT_PER_MIN", 120),
        register_limit_per_min: limit("REGISTER_LIMIT_PER_MIN", 10),
        cors_origins: env("CORS_ORIGINS", Some("http://localhost:3001,http://127.0.0.1:3001"))
            .split(',')
            .map(|o| o.trim().to_string())
            .filter(|o| !o.is_empty())
            .collect(),
        web_base_url: std::env::var("PUBLIC_WEB_URL")
            .ok()
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| u.starts_with("https://") || u.starts_with("http://")),
        ..ApiConfig::default()
    };
    let rates = escrow_api::rates::from_setting(&env("FIAT_PROVIDER", Some("off"))).unwrap_or_else(|e| {
        eprintln!("FIAT_PROVIDER: {e}");
        std::process::exit(2);
    });
    if !rates.currencies().is_empty() {
        tracing::info!(source = %rates.name(), currencies = rates.currencies().len(), "fiat values on (display only)");
    }
    let web_base_url = config.web_base_url.clone();
    let mut state = AppState::new(service.clone(), store.clone(), config, Arc::new(runner::unix_now)).with_rates(rates.clone());

    // Webhooks need the credential key to seal signing secrets, so they are on only when it is set.
    let webhook_sender: Arc<dyn escrow_api::webhooks::Sender> = Arc::new(escrow_api::webhooks::HttpSender { allow_private });
    let dispatcher = webhook_key.clone().map(|key| {
        escrow_api::webhooks::Dispatcher::new(store.clone(), key, webhook_sender.clone(), web_base_url)
    });
    if let Some(key) = webhook_key {
        state = state.with_webhooks(escrow_api::WebhookSupport { key, sender: webhook_sender });
    }
    let payouts =
        money.payout_wallet.map(|w| Arc::new(PayoutService::new(w, store.clone()).with_limits(money.payout_limits)));
    let ramp = money.ramp_wallet.map(|w| {
        let config = RampConfig { fee_bps: money.ramp_fee_bps, ..RampConfig::default() };
        if !rates.currencies().contains(&config.currency) {
            tracing::warn!("the naira ramp needs a price for NGN: set FIAT_PROVIDER to coingecko or fixed:NGN=...");
        }
        Arc::new(RampService::new(Arc::new(DemoRamp::new(w)), payouts.clone(), store.clone(), config))
    });
    if let Some(p) = &payouts {
        state = state.with_payouts(p.clone());
    }
    if let Some(r) = &ramp {
        state = state.with_ramp(r.clone());
    }
    let app = router(state);

    // Keep escrows in step with the node, and cancel holds at their deadline.
    let events = tokio::spawn({
        let service = service.clone();
        async move { runner::run_event_loop(&service, runner::EventLoopConfig::default(), std::future::pending()).await }
    });
    let scheduler = tokio::spawn({
        let service = service.clone();
        async move { runner::run_scheduler(&service, Duration::from_secs(1), std::future::pending()).await }
    });

    let webhook_task = dispatcher.map(|d| tokio::spawn(d.run(Duration::from_secs(2), runner::unix_now)));

    // Settle withdrawals from what the node reports, and move naira orders on.
    let money_task = (payouts.is_some() || ramp.is_some()).then(|| {
        tokio::spawn(async move {
            runner::run_payouts(payouts.as_deref(), ramp.as_deref(), Duration::from_secs(2), std::future::pending()).await
        })
    });

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "toshi-server listening");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    events.abort();
    scheduler.abort();
    if let Some(t) = webhook_task {
        t.abort();
    }
    if let Some(t) = money_task {
        t.abort();
    }
    Ok(())
}
