//! Bearer-token authentication and per-escrow authorization.
//!
//! Tokens are random 256-bit values shown once at registration; only their SHA-256 is stored.

use axum::{extract::FromRequestParts, http::request::Parts};
use escrow_core::{
    adapter::NodeDirectory,
    model::{ApiKey, Escrow, Role, Scope, User},
    state::EscrowState,
    store::{Store, UserStore},
};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::{error::ApiError, AppState};

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn new_token() -> String {
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    format!("tw_{}", hex::encode(b))
}

/// An API key: `tk_` and 256 random bits. The prefix tells it apart from an account token (`tw_`).
pub fn new_api_key() -> String {
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    format!("tk_{}", hex::encode(b))
}

pub fn new_api_key_id() -> String {
    let mut b = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut b);
    format!("key_{}", hex::encode(b))
}

pub fn new_user_id() -> String {
    let mut b = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut b);
    format!("u_{}", hex::encode(b))
}

/// The caller's relationship to one escrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Party {
    Buyer,
    Seller,
    Arbiter,
    Operator,
}

impl Party {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Buyer => "buyer",
            Self::Seller => "seller",
            Self::Arbiter => "arbiter",
            Self::Operator => "operator",
        }
    }
}

/// An open offer (no buyer yet) can be viewed and accepted by any other user who has its link.
pub fn is_prospective_buyer(escrow: &Escrow, user: &User) -> bool {
    escrow.buyer_id.is_none()
        && escrow.state == EscrowState::Created
        && user.role == Role::User
        && user.id != escrow.seller_id
}

/// `None` means the user has no standing on this escrow and must not learn that it exists.
pub fn party_of(escrow: &Escrow, user: &User) -> Option<Party> {
    if escrow.buyer_id.as_deref() == Some(user.id.as_str()) {
        Some(Party::Buyer)
    } else if user.id == escrow.seller_id {
        Some(Party::Seller)
    } else if escrow.arbiter_id.as_deref() == Some(user.id.as_str()) {
        Some(Party::Arbiter)
    } else if user.role == Role::Operator {
        Some(Party::Operator)
    } else {
        None
    }
}

/// How often a key's "last used" time is written, so a busy integration does not write on every call.
const TOUCH_EVERY_SECS: i64 = 60;

/// Who is calling: the account holder (account token) or software acting for them (API key).
async fn resolve<A, S>(parts: &Parts, state: &AppState<A, S>) -> Result<(User, Option<ApiKey>), ApiError>
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + Clone + 'static,
{
    let token = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(ApiError::unauthorized)?;
    let hash = hash_token(token);
    if let Some(user) = state.users.user_by_token_hash(&hash).await? {
        return Ok((user, None));
    }
    // Not an account token: it may be an API key. A revoked key is simply unknown.
    let key = state.users.api_key_by_hash(&hash).await?.filter(|k| k.revoked_at.is_none()).ok_or_else(ApiError::unauthorized)?;
    let user = state.users.get_user(&key.user_id).await?.ok_or_else(ApiError::unauthorized)?;
    let now = (state.now)();
    if key.last_used_at.is_none_or(|t| now - t >= TOUCH_EVERY_SECS) {
        state.users.touch_api_key(&key.id, now).await?;
    }
    Ok((user, Some(key)))
}

/// The authenticated caller, by account token only. An API key is refused: keys can reach only the
/// endpoints that ask for [`ApiAuth`] and a scope, so everything else stays closed to them by default.
pub struct Auth(pub User);

impl<A, S> FromRequestParts<AppState<A, S>> for Auth
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + Clone + 'static,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState<A, S>) -> Result<Self, ApiError> {
        match resolve(parts, state).await? {
            (user, None) => Ok(Auth(user)),
            (_, Some(_)) => Err(ApiError::forbidden("an API key cannot call this endpoint; use the account token")),
        }
    }
}

/// The authenticated caller, by account token or by API key. Call [`ApiAuth::require`] with the scope
/// the endpoint needs; an account token always passes.
pub struct ApiAuth {
    pub user: User,
    pub key: Option<ApiKey>,
}

impl ApiAuth {
    pub fn require(&self, scope: Scope) -> Result<(), ApiError> {
        match &self.key {
            Some(k) if !k.allows(scope) => {
                Err(ApiError::forbidden(format!("this API key does not have the {} scope", scope.as_str())))
            }
            _ => Ok(()),
        }
    }
}

impl<A, S> FromRequestParts<AppState<A, S>> for ApiAuth
where
    A: NodeDirectory + 'static,
    S: Store + UserStore + Clone + 'static,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState<A, S>) -> Result<Self, ApiError> {
        let (user, key) = resolve(parts, state).await?;
        Ok(ApiAuth { user, key })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use escrow_core::state::EscrowState;

    fn escrow() -> Escrow {
        Escrow {
            id: "e".into(),
            buyer_id: Some("b".into()),
            arbiter_id: Some("j".into()),
            arbiter_key: None,
            arbiter_sealed: None,
            arbiter_confirmed_at: None,
            delivered_at: None,
            node: escrow_core::model::NodeKind::Shared,
            fiat: None,
            reference: None,
            return_url: None,
            seller_id: "s".into(),
            amount_sats: 1,
            description: String::new(),
            payment_hash: None,
            invoice: None,
            invoice_expires_at: None,
            state: EscrowState::Created,
            deadline_at: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn user(id: &str, role: Role) -> User {
        User { id: id.into(), role, created_at: 0 }
    }

    #[test]
    fn parties_are_resolved_per_escrow() {
        let e = escrow();
        assert_eq!(party_of(&e, &user("b", Role::User)), Some(Party::Buyer));
        assert_eq!(party_of(&e, &user("s", Role::User)), Some(Party::Seller));
        assert_eq!(party_of(&e, &user("j", Role::User)), Some(Party::Arbiter));
        assert_eq!(party_of(&e, &user("op", Role::Operator)), Some(Party::Operator));
        assert_eq!(party_of(&e, &user("x", Role::User)), None);
    }

    #[test]
    fn open_offers_can_be_seen_by_other_users_but_are_nobodys_until_accepted() {
        let mut e = escrow();
        e.buyer_id = None;
        assert_eq!(party_of(&e, &user("x", Role::User)), None, "no standing until they accept");
        assert!(is_prospective_buyer(&e, &user("x", Role::User)));
        assert!(!is_prospective_buyer(&e, &user("s", Role::User)), "the seller is not a prospect");
        assert!(!is_prospective_buyer(&e, &user("op", Role::Operator)));
        e.state = EscrowState::InvoiceIssued;
        assert!(!is_prospective_buyer(&e, &user("x", Role::User)), "only while still CREATED");
    }

    #[test]
    fn tokens_are_long_random_and_hashed_for_storage() {
        let (a, b) = (new_token(), new_token());
        assert_ne!(a, b);
        assert_eq!(a.len(), 3 + 64);
        assert_eq!(hash_token(&a).len(), 64);
        assert_ne!(hash_token(&a), a);
    }
}
