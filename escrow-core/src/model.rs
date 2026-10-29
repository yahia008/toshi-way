//! Core data types.

use std::fmt;

use sha2::{Digest, Sha256};

use crate::{fiat::FiatQuote, state::EscrowState};

pub type PaymentHash = [u8; 32];

/// The release secret. Never stored and never logged: `Debug` is redacted and there is no
/// `Display`, so it cannot end up in a log line by accident.
pub struct Preimage([u8; 32]);

impl Preimage {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parse the 64-hex-character form. Anything that is not exactly 32 bytes is rejected,
    /// so short codes can never be accepted as a release secret.
    pub fn from_hex(s: &str) -> Option<Self> {
        let bytes = hex::decode(s).ok()?;
        Some(Self(bytes.try_into().ok()?))
    }

    pub fn hash(&self) -> PaymentHash {
        Sha256::digest(self.0).into()
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for Preimage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Preimage(<redacted>)")
    }
}

/// Whose Lightning node holds an escrow's invoice, and therefore receives the money on release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// The seller's own node: on release the sats go straight to the seller. Non-custodial.
    Own,
    /// The service's shared node: the operator receives the sats and owes them to the seller.
    /// Custodial after release; offered only when the operator enables it.
    Shared,
}

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Own => "own",
            Self::Shared => "shared",
        }
    }
}

impl std::str::FromStr for NodeKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "own" => Ok(Self::Own),
            "shared" => Ok(Self::Shared),
            other => Err(format!("unknown node kind '{other}'")),
        }
    }
}

/// How to reach a seller's own Lightning node. The macaroon is stored encrypted; the service
/// that encrypts it is the only one that can read it back.
#[derive(Clone, PartialEq, Eq)]
pub struct NodeConnection {
    pub user_id: String,
    /// `https://host:port` of the node's gRPC interface.
    pub address: String,
    /// The node's TLS certificate (PEM). It is public, and the connection is pinned to it.
    pub tls_cert: String,
    /// Nonce followed by ciphertext of the invoice macaroon.
    pub macaroon_enc: Vec<u8>,
    pub updated_at: i64,
}

impl fmt::Debug for NodeConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeConnection")
            .field("user_id", &self.user_id)
            .field("address", &self.address)
            .field("macaroon_enc", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Escrow {
    pub id: String,
    /// `None` for an open offer that nobody has accepted yet; accepting it sets the buyer.
    pub buyer_id: Option<String>,
    pub seller_id: String,
    /// A trusted third party both sides agreed on, who decides if there is a dispute.
    pub arbiter_id: Option<String>,
    /// The arbiter's public key (base64 SPKI) as it was when the escrow was created. Recorded
    /// here so the key the buyer encrypted to cannot be swapped afterwards.
    pub arbiter_key: Option<String>,
    /// The buyer's release secret encrypted to `arbiter_key`. Opaque to the server, which cannot
    /// read it; only the arbiter can. Present once the buyer has accepted.
    pub arbiter_sealed: Option<String>,
    /// When the arbiter confirmed they can decrypt `arbiter_sealed` and that it matches the
    /// payment hash. Until then the seller should not count on arbitration.
    pub arbiter_confirmed_at: Option<i64>,
    /// When the seller said the goods or work were delivered. A claim, not a payment: only the
    /// buyer's secret releases the money.
    pub delivered_at: Option<i64>,
    /// Which node holds this escrow's invoice. Fixed at creation, so changing the seller's node
    /// settings later cannot strand an escrow that is in flight.
    pub node: NodeKind,
    pub amount_sats: u64,
    /// What the sum was worth in a fiat currency when the offer was made. Display only: nothing in
    /// the escrow logic reads it, and it never changes after creation.
    pub fiat: Option<FiatQuote>,
    /// The seller's own order number (or any label), so a business can match this escrow to its
    /// own records. Opaque to the service and set at creation.
    pub reference: Option<String>,
    /// Where the buyer is offered a link back to, for a business's checkout. Shown as a link the
    /// buyer chooses to follow, never followed automatically. Set at creation.
    pub return_url: Option<String>,
    pub description: String,
    pub payment_hash: Option<PaymentHash>,
    /// The BOLT11 string the buyer pays. Not for ordinary logs or public responses.
    pub invoice: Option<String>,
    pub invoice_expires_at: Option<i64>,
    pub state: EscrowState,
    /// Settle-by time (unix seconds). Releases are accepted before it; the scheduler cancels at it.
    pub deadline_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub escrow_id: String,
    pub actor: String,
    pub action: String,
    pub from_state: EscrowState,
    pub to_state: EscrowState,
    pub reason: String,
    pub created_at: i64,
}

/// State of a hold invoice as reported by the node. The node is authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeInvoiceState {
    Open,
    Accepted,
    Settled,
    Canceled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInvoice {
    pub state: NodeInvoiceState,
    /// Block height at which the held HTLC expires, once a payment is held.
    pub htlc_expiry_height: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Operator,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Operator => "operator",
        }
    }
}

impl std::str::FromStr for Role {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user" => Ok(Self::User),
            "operator" => Ok(Self::Operator),
            other => Err(format!("unknown role '{other}'")),
        }
    }
}

/// Whether someone is the buyer or seller of an escrow is per escrow, not part of the account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: String,
    pub role: Role,
    pub created_at: i64,
}

/// An audit entry with its position in the log. The position only ever grows, so it serves as a
/// cursor for anything that works through the log in order, such as webhook delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    pub seq: i64,
    pub entry: AuditEntry,
}

/// Where a business wants to be told about changes to its escrows, and how delivery is going.
#[derive(Clone, PartialEq, Eq)]
pub struct Webhook {
    pub id: String,
    pub user_id: String,
    pub url: String,
    /// The signing secret, sealed by the application. The database never holds it in the clear.
    pub signing_key_enc: Vec<u8>,
    pub created_at: i64,
    /// The last audit-log position delivered. Deliveries start after this, in order.
    pub last_seq: i64,
    /// Failed tries in a row for the next event; 0 when delivery is healthy.
    pub attempts: u32,
    pub next_attempt_at: i64,
    /// When delivery began failing, while it still is; `None` when healthy.
    pub failing_since: Option<i64>,
    /// Set when delivery has failed for too long and the endpoint was switched off.
    pub disabled_at: Option<i64>,
    pub last_attempt_at: Option<i64>,
    /// How the last try went: an HTTP status such as "200", or a short reason such as "timeout".
    pub last_status: Option<String>,
}

impl std::fmt::Debug for Webhook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Webhook").field("id", &self.id).field("url", &self.url).field("signing_key_enc", &"<sealed>").finish_non_exhaustive()
    }
}

/// What a delivery attempt changed about an endpoint's progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookProgress {
    pub last_seq: i64,
    pub attempts: u32,
    pub next_attempt_at: i64,
    pub failing_since: Option<i64>,
    pub disabled_at: Option<i64>,
    pub last_attempt_at: Option<i64>,
    pub last_status: Option<String>,
}

/// What an API key is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// List and read the account's escrows and their history.
    EscrowsRead,
    /// Create escrows as the seller, and cancel them (after the buyer has paid, that refunds the buyer).
    EscrowsWrite,
}

impl Scope {
    pub const ALL: [Scope; 2] = [Scope::EscrowsRead, Scope::EscrowsWrite];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::EscrowsRead => "escrows:read",
            Self::EscrowsWrite => "escrows:write",
        }
    }
}

impl std::str::FromStr for Scope {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL.into_iter().find(|x| x.as_str() == s).ok_or_else(|| format!("unknown scope '{s}'"))
    }
}

/// A revocable credential for a business's software. The key itself is shown once at creation;
/// only its hash is stored, so what is kept here is enough to recognise it but not to use it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKey {
    pub id: String,
    pub user_id: String,
    pub name: String,
    /// The last few characters of the key, so a person can tell their keys apart.
    pub last_chars: String,
    pub scopes: Vec<Scope>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

impl ApiKey {
    pub fn allows(&self, scope: Scope) -> bool {
        self.revoked_at.is_none() && self.scopes.contains(&scope)
    }
}

/// A node report about an escrow's invoice, kept for the audit trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeEventRecord {
    pub escrow_id: String,
    pub node_invoice_state: NodeInvoiceState,
    /// Hash of the raw event, so repeats can be recognised without storing the payload.
    pub raw_event_hash: String,
    pub received_at: i64,
}

impl NodeInvoiceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "OPEN",
            Self::Accepted => "ACCEPTED",
            Self::Settled => "SETTLED",
            Self::Canceled => "CANCELED",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvoiceEvent {
    pub payment_hash: PaymentHash,
    pub state: NodeInvoiceState,
}

/// Where a withdrawal sends the sats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawalKind {
    /// To a Lightning invoice the seller supplied: their own wallet.
    Lightning,
    /// To the ramp provider's invoice, which pays naira into the seller's bank account in return.
    Bank,
}

impl WithdrawalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lightning => "lightning",
            Self::Bank => "bank",
        }
    }
}

impl std::str::FromStr for WithdrawalKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "lightning" => Ok(Self::Lightning),
            "bank" => Ok(Self::Bank),
            other => Err(format!("unknown withdrawal kind '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawalState {
    /// The node is paying it. The amount and the most the fee can be are held back from the balance.
    Pending,
    Succeeded,
    /// Nothing left the node; the amount is back in the balance.
    Failed,
}

impl WithdrawalState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

impl std::str::FromStr for WithdrawalState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            other => Err(format!("unknown withdrawal state '{other}'")),
        }
    }
}

/// A payout from the shared node of sats the operator owes a seller. Whether it went through is
/// what the node reports about the payment, never what anyone claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withdrawal {
    pub id: String,
    pub user_id: String,
    pub kind: WithdrawalKind,
    pub amount_sats: u64,
    /// The most the routing fee may come to. Held back from the balance while the payment is pending.
    pub fee_limit_sats: u64,
    /// The routing fee actually paid, once the payment succeeded.
    pub fee_sats: Option<u64>,
    /// The BOLT11 invoice being paid.
    pub invoice: String,
    pub payment_hash: PaymentHash,
    pub state: WithdrawalState,
    /// Why it failed, in the node's words.
    pub failure: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Withdrawal {
    /// What it takes out of the balance in its current state.
    pub fn debit(&self) -> u64 {
        match self.state {
            WithdrawalState::Pending => self.amount_sats + self.fee_limit_sats,
            WithdrawalState::Succeeded => self.amount_sats + self.fee_sats.unwrap_or(0),
            WithdrawalState::Failed => 0,
        }
    }
}

/// A bank account in the ramp's currency: where a seller's payout or a buyer's refund goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BankAccount {
    /// The bank's code in the national payment system (CBN/NIBSS codes for Nigeria).
    pub bank_code: String,
    pub bank_name: String,
    /// The 10-digit NUBAN.
    pub account_number: String,
    /// The holder's name as the bank reports it.
    pub account_name: String,
}

/// Where a buyer sends money for an on-ramp: a bank transfer to the provider, with a reference that
/// ties the transfer to the order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepositInstructions {
    pub bank_name: String,
    pub account_number: String,
    pub account_name: String,
    pub reference: String,
    /// The transfer must arrive before this, so the escrow's invoice can still be paid.
    pub expires_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RampDirection {
    /// A buyer pays an escrow in local currency; the provider pays the Lightning invoice.
    On,
    /// A seller sends sats from their balance to the provider, who pays local currency to their bank.
    Off,
}

impl RampDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "onramp",
            Self::Off => "offramp",
        }
    }
}

impl std::str::FromStr for RampDirection {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "onramp" => Ok(Self::On),
            "offramp" => Ok(Self::Off),
            other => Err(format!("unknown ramp direction '{other}'")),
        }
    }
}

/// Where a ramp order has got to. On-ramp orders go AwaitingDeposit -> DepositReceived -> Paying ->
/// Funded -> Complete, or end in Expired, or (once money arrived but the escrow did not pay the
/// seller) Refunding -> Refunded. Off-ramp orders go SendingSats -> PayingOut -> PaidOut, or Failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RampState {
    AwaitingDeposit,
    DepositReceived,
    Paying,
    Funded,
    Complete,
    Refunding,
    Refunded,
    Expired,
    SendingSats,
    PayingOut,
    PaidOut,
    Failed,
}

impl RampState {
    pub const ALL: [RampState; 12] = [
        Self::AwaitingDeposit,
        Self::DepositReceived,
        Self::Paying,
        Self::Funded,
        Self::Complete,
        Self::Refunding,
        Self::Refunded,
        Self::Expired,
        Self::SendingSats,
        Self::PayingOut,
        Self::PaidOut,
        Self::Failed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingDeposit => "awaiting_deposit",
            Self::DepositReceived => "deposit_received",
            Self::Paying => "paying",
            Self::Funded => "funded",
            Self::Complete => "complete",
            Self::Refunding => "refunding",
            Self::Refunded => "refunded",
            Self::Expired => "expired",
            Self::SendingSats => "sending_sats",
            Self::PayingOut => "paying_out",
            Self::PaidOut => "paid_out",
            Self::Failed => "failed",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Refunded | Self::Expired | Self::PaidOut | Self::Failed)
    }
}

impl std::str::FromStr for RampState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL.into_iter().find(|x| x.as_str() == s).ok_or_else(|| format!("unknown ramp state '{s}'"))
    }
}

/// One conversion between local currency and sats, through the ramp provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RampOrder {
    pub id: String,
    pub user_id: String,
    pub direction: RampDirection,
    /// The escrow an on-ramp pays for.
    pub escrow_id: Option<String>,
    /// The withdrawal that sends an off-ramp's sats to the provider.
    pub withdrawal_id: Option<String>,
    pub amount_sats: u64,
    /// ISO 4217 code, upper case.
    pub currency: String,
    /// What the person pays (on-ramp, fee included) or receives (off-ramp, fee taken), in minor units.
    pub fiat_minor: i64,
    /// The provider's fee, in minor units, already counted in `fiat_minor`.
    pub fee_minor: i64,
    pub rate_minor_per_btc: i64,
    /// Where a payout (off-ramp) or a refund (on-ramp) is sent.
    pub bank: BankAccount,
    /// Where the buyer sends money, for an on-ramp.
    pub deposit: Option<DepositInstructions>,
    pub provider: String,
    /// The provider's reference for the money it sent out (payout or refund).
    pub fiat_reference: Option<String>,
    pub state: RampState,
    /// The latest thing worth telling the person, such as why a payout is being retried.
    pub note: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}
