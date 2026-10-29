//! `/livez` says the process is up without asking the Lightning node (in-memory store).

use std::sync::{atomic::AtomicI64, Arc};

use axum::{body::Body, http::Request};
use escrow_api::{router, ApiConfig, AppState};
use escrow_core::{mock::MockLightning, service::EscrowService, store::MemoryStore, timing::TimingConfig};
use tower::ServiceExt;

#[tokio::test]
async fn livez_is_ok_even_when_the_node_is_down() {
    let node = MockLightning::new();
    let store = Arc::new(MemoryStore::new());
    let service = Arc::new(EscrowService::new(node.clone(), store.clone(), TimingConfig::default()));
    let clock = Arc::new(AtomicI64::new(1_000_000));
    let now = Arc::new(move || clock.load(std::sync::atomic::Ordering::SeqCst));
    let app = router(AppState::new(service, store, ApiConfig::default(), now));

    let live = app.clone().oneshot(Request::get("/livez").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(live.status(), 200);
}
