//! LND-backed [`LightningAdapter`].
//!
//! Every node call carries a timeout. `settle_invoice` and `cancel_invoice` are idempotent: if
//! LND refuses because the invoice is already in the wanted state, that counts as success.

pub mod address;
pub mod creds;
pub mod directory;
pub mod runner;
pub mod wallet;

use std::{path::Path, time::Duration};

use async_trait::async_trait;
use escrow_core::{
    adapter::{AdapterError, HoldInvoiceRequest, LightningAdapter, NodeDirectory},
    model::{InvoiceEvent, NodeInvoice, NodeInvoiceState, NodeKind, PaymentHash, Preimage},
};
use fedimint_tonic_lnd::{
    connect,
    invoicesrpc::{
        lookup_invoice_msg::InvoiceRef, AddHoldInvoiceRequest, CancelInvoiceMsg, LookupInvoiceMsg,
        SettleInvoiceMsg, SubscribeSingleInvoiceRequest,
    },
    lnrpc::{invoice::InvoiceState, GetInfoRequest, Invoice},
    tonic::{Code, Status, Streaming},
    Client,
};

/// rustls needs a process-wide crypto provider before any TLS connection. Safe to call twice.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[derive(Clone)]
pub struct LndAdapter {
    client: Client,
    timeout: Duration,
}

fn map_status(status: Status) -> AdapterError {
    match status.code() {
        Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled | Code::Aborted | Code::ResourceExhausted => {
            AdapterError::Unavailable(status.message().to_string())
        }
        _ => AdapterError::Rejected(status.message().to_string()),
    }
}

fn node_state(state: i32) -> Option<NodeInvoiceState> {
    match InvoiceState::try_from(state).ok()? {
        InvoiceState::Open => Some(NodeInvoiceState::Open),
        InvoiceState::Accepted => Some(NodeInvoiceState::Accepted),
        InvoiceState::Settled => Some(NodeInvoiceState::Settled),
        InvoiceState::Canceled => Some(NodeInvoiceState::Canceled),
    }
}

fn sha2_hash(bytes: &[u8]) -> PaymentHash {
    escrow_core::model::Preimage::from_bytes(bytes.try_into().expect("32 bytes")).hash()
}

fn to_hash(bytes: &[u8]) -> Option<PaymentHash> {
    bytes.try_into().ok()
}

impl LndAdapter {
    /// `address` is `https://host:port`; the macaroon should be limited to invoice operations
    /// in production.
    pub async fn connect(
        address: &str,
        tls_cert: impl AsRef<Path> + Into<std::path::PathBuf> + std::fmt::Debug,
        macaroon: impl AsRef<Path> + Into<std::path::PathBuf> + std::fmt::Debug,
        timeout: Duration,
    ) -> Result<Self, AdapterError> {
        install_crypto_provider();
        let client = tokio::time::timeout(timeout, connect(address.to_string(), tls_cert, macaroon))
            .await
            .map_err(|_| AdapterError::Unavailable("timed out connecting to the node".into()))?
            .map_err(|e| AdapterError::Unavailable(e.to_string()))?;
        Ok(Self { client, timeout })
    }

    /// Connect with credentials held in memory (a seller's node, from the database). The client
    /// library only reads credentials from files, so they are written to a private temporary
    /// directory, read by the connection, and deleted again straight away. The connection is
    /// pinned to exactly this certificate, so a different server cannot answer in the node's place.
    pub async fn connect_from_memory(
        address: &str,
        tls_cert_pem: &str,
        macaroon: &[u8],
        timeout: Duration,
    ) -> Result<Self, AdapterError> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        use std::io::Write;

        let mut nonce = [0u8; 8];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
        let dir = std::env::temp_dir().join(format!("toshi-node-{}", hex::encode(nonce)));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| AdapterError::Unavailable(format!("could not prepare credentials: {e}")))?;
        let write = |name: &str, bytes: &[u8]| -> std::io::Result<std::path::PathBuf> {
            let path = dir.join(name);
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
            f.write_all(bytes)?;
            Ok(path)
        };
        let result = async {
            let cert = write("tls.cert", tls_cert_pem.as_bytes()).map_err(|e| e.to_string())?;
            let mac = write("node.macaroon", macaroon).map_err(|e| e.to_string())?;
            Self::connect(address, cert, mac, timeout).await.map_err(|e| e.to_string())
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        result.map_err(AdapterError::Unavailable)
    }

    /// Prove this connection can do what escrows need: the node answers and is synced, can read
    /// invoices, and can create and cancel one. A throwaway hold invoice is made and cancelled.
    pub async fn check_invoice_access(&self) -> Result<(), AdapterError> {
        LightningAdapter::ping(self).await?;
        let mut probe = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut probe);
        let hash: PaymentHash = sha2_hash(&probe);
        self.create_hold_invoice(HoldInvoiceRequest {
            payment_hash: hash,
            amount_sats: 1,
            memo: "toshi-way connection check".into(),
            expiry_secs: 60,
            final_cltv_delta: 80,
        })
        .await?;
        self.cancel_invoice(&hash).await?;
        self.lookup_invoice(&hash).await?;
        Ok(())
    }

    /// The bitcoin network the node runs on, as LND names it: "mainnet", "testnet", "signet", "regtest".
    pub async fn network(&self) -> Result<String, AdapterError> {
        let mut client = self.client.clone();
        let info = self.call(async { client.lightning().get_info(GetInfoRequest {}).await }).await?.into_inner();
        info.chains
            .first()
            .map(|c| c.network.clone())
            .ok_or_else(|| AdapterError::Unavailable("the node did not say which network it is on".into()))
    }

    /// Wrap a call in the adapter's timeout.
    async fn call<T>(
        &self,
        fut: impl std::future::Future<Output = Result<T, Status>>,
    ) -> Result<T, AdapterError> {
        match tokio::time::timeout(self.timeout, fut).await {
            Ok(res) => res.map_err(map_status),
            Err(_) => Err(AdapterError::Unavailable("node call timed out".into())),
        }
    }

    /// Stream state changes of one invoice. LND sends the current state first, so a subscriber
    /// that starts late still learns what it missed.
    ///
    /// This is per invoice on purpose: the node-wide `SubscribeInvoices` stream sends no response
    /// headers until its first event (so a call timeout would kill it) and does not announce a
    /// hold invoice becoming ACCEPTED. The stream covers one connection only; see
    /// [`runner::run_event_loop`] for reconnecting.
    pub async fn subscribe_invoice(&self, payment_hash: &PaymentHash) -> Result<InvoiceEvents, AdapterError> {
        let mut client = self.client.clone();
        let stream = self
            .call(async {
                client
                    .invoices()
                    .subscribe_single_invoice(SubscribeSingleInvoiceRequest { r_hash: payment_hash.to_vec() })
                    .await
                    .map(|r| r.into_inner())
            })
            .await?;
        Ok(InvoiceEvents(stream))
    }
}

pub struct InvoiceEvents(Streaming<Invoice>);

impl InvoiceEvents {
    /// The next update. `Ok(None)` means the node closed the stream.
    pub async fn next(&mut self) -> Result<Option<InvoiceEvent>, AdapterError> {
        loop {
            let Some(inv) = self.0.message().await.map_err(map_status)? else {
                return Ok(None);
            };
            if let (Some(payment_hash), Some(state)) = (to_hash(&inv.r_hash), node_state(inv.state)) {
                return Ok(Some(InvoiceEvent { payment_hash, state }));
            }
        }
    }
}

#[async_trait]
impl LightningAdapter for LndAdapter {
    async fn create_hold_invoice(&self, req: HoldInvoiceRequest) -> Result<String, AdapterError> {
        let mut client = self.client.clone();
        let res = self
            .call(async {
                client
                    .invoices()
                    .add_hold_invoice(AddHoldInvoiceRequest {
                        memo: req.memo,
                        hash: req.payment_hash.to_vec(),
                        value: i64::try_from(req.amount_sats).unwrap_or(i64::MAX),
                        expiry: req.expiry_secs,
                        cltv_expiry: u64::from(req.final_cltv_delta),
                        ..Default::default()
                    })
                    .await
            })
            .await?
            .into_inner();
        Ok(res.payment_request)
    }

    async fn settle_invoice(&self, preimage: &Preimage) -> Result<(), AdapterError> {
        let mut client = self.client.clone();
        let result = self
            .call(async {
                client
                    .invoices()
                    .settle_invoice(SettleInvoiceMsg { preimage: preimage.as_bytes().to_vec() })
                    .await
            })
            .await;
        match result {
            Ok(_) => Ok(()),
            // Already settled counts as success.
            Err(err) => match self.lookup_invoice(&preimage.hash()).await {
                Ok(Some(inv)) if inv.state == NodeInvoiceState::Settled => Ok(()),
                _ => Err(err),
            },
        }
    }

    async fn cancel_invoice(&self, payment_hash: &PaymentHash) -> Result<(), AdapterError> {
        let mut client = self.client.clone();
        let result = self
            .call(async {
                client
                    .invoices()
                    .cancel_invoice(CancelInvoiceMsg { payment_hash: payment_hash.to_vec() })
                    .await
            })
            .await;
        match result {
            Ok(_) => Ok(()),
            // Already cancelled counts as success.
            Err(err) => match self.lookup_invoice(payment_hash).await {
                Ok(Some(inv)) if inv.state == NodeInvoiceState::Canceled => Ok(()),
                _ => Err(err),
            },
        }
    }

    async fn ping(&self) -> Result<(), AdapterError> {
        let mut client = self.client.clone();
        let info = self.call(async { client.lightning().get_info(GetInfoRequest {}).await }).await?.into_inner();
        if info.synced_to_chain {
            Ok(())
        } else {
            Err(AdapterError::Unavailable("node is not synced to the chain".into()))
        }
    }

    async fn lookup_invoice(&self, payment_hash: &PaymentHash) -> Result<Option<NodeInvoice>, AdapterError> {
        let mut client = self.client.clone();
        let res = self
            .call(async {
                client
                    .invoices()
                    .lookup_invoice_v2(LookupInvoiceMsg {
                        invoice_ref: Some(InvoiceRef::PaymentHash(payment_hash.to_vec())),
                        ..Default::default()
                    })
                    .await
            })
            .await;
        let inv = match res {
            Ok(r) => r.into_inner(),
            Err(AdapterError::Rejected(msg)) if msg.contains("unable to locate invoice") || msg.contains("not found") => {
                return Ok(None)
            }
            Err(e) => return Err(e),
        };
        let state = node_state(inv.state)
            .ok_or_else(|| AdapterError::Rejected(format!("unknown invoice state {}", inv.state)))?;
        Ok(Some(NodeInvoice {
            state,
            htlc_expiry_height: inv.htlcs.iter().map(|h| h.expiry_height as u32).min(),
        }))
    }
}

/// A single node serving every seller: how the service ran before sellers could bring their own,
/// and how the adapter is used directly in tests.
#[async_trait]
impl NodeDirectory for LndAdapter {
    type Node = LndAdapter;

    async fn kind_for_new(&self, _seller_id: &str) -> Result<NodeKind, AdapterError> {
        Ok(NodeKind::Shared)
    }

    async fn node(&self, _seller_id: &str, _kind: NodeKind) -> Result<LndAdapter, AdapterError> {
        Ok(self.clone())
    }

    async fn ping(&self) -> Result<(), AdapterError> {
        LightningAdapter::ping(self).await
    }
}
