//! The withdrawal PIN. An account token is all it takes to sign in, so whoever steals one could
//! otherwise send the balance anywhere. Every withdrawal also asks for a 6-digit PIN that only the
//! seller knows. The server keeps an Argon2 hash of it, counts wrong guesses and locks it for a while
//! after too many, so it cannot be guessed through the API.

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::http::StatusCode;
use escrow_core::store::PayoutStore;

use crate::error::ApiError;

/// Wrong guesses allowed before the PIN locks.
pub const MAX_FAILURES: u32 = 5;
/// How long a lock lasts. After it, each further wrong guess locks it again.
pub const LOCK_SECS: i64 = 15 * 60;

/// Six digits, not all the same and not a run like 123456.
pub fn validate(pin: &str) -> Result<(), ApiError> {
    let bad = |m: &str| Err(ApiError::bad_request("invalid_pin", m.to_string()));
    if pin.len() != 6 || !pin.bytes().all(|b| b.is_ascii_digit()) {
        return bad("the PIN is exactly 6 digits");
    }
    let d: Vec<i8> = pin.bytes().map(|b| (b - b'0') as i8).collect();
    let steps: Vec<i8> = d.windows(2).map(|w| w[1] - w[0]).collect();
    if steps.iter().all(|&s| s == steps[0]) && steps[0].abs() <= 1 {
        return bad("that PIN is too easy to guess; avoid repeated digits and runs like 123456");
    }
    Ok(())
}

/// Hashing is deliberately slow, so it runs off the async threads.
pub async fn hash(pin: String) -> Result<String, ApiError> {
    tokio::task::spawn_blocking(move || {
        Argon2::default().hash_password(pin.as_bytes(), &SaltString::generate(&mut OsRng)).map(|h| h.to_string())
    })
    .await
    .ok()
    .and_then(Result::ok)
    .ok_or_else(|| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "could not store the PIN"))
}

async fn matches(stored: String, pin: String) -> bool {
    tokio::task::spawn_blocking(move || {
        PasswordHash::new(&stored).is_ok_and(|h| Argon2::default().verify_password(pin.as_bytes(), &h).is_ok())
    })
    .await
    .unwrap_or(false)
}

/// Check `pin` for `user_id` before money leaves. Every wrong guess is counted.
pub async fn check<S: PayoutStore>(store: &S, user_id: &str, pin: Option<&str>, now: i64) -> Result<(), ApiError> {
    let Some(record) = store.get_withdrawal_pin(user_id).await? else {
        return Err(ApiError::new(StatusCode::CONFLICT, "pin_not_set", "set a withdrawal PIN first"));
    };
    if let Some(until) = record.locked_until.filter(|&t| t > now) {
        let minutes = (until - now + 59) / 60;
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "pin_locked",
            format!("too many wrong PINs; try again in {minutes} minutes"),
        ));
    }
    let pin = pin.map(str::trim).filter(|p| !p.is_empty());
    let Some(pin) = pin else {
        return Err(ApiError::bad_request("pin_required", "enter your withdrawal PIN"));
    };
    if matches(record.pin_hash, pin.to_string()).await {
        store.withdrawal_pin_ok(user_id).await?;
        return Ok(());
    }
    store.withdrawal_pin_failed(user_id, MAX_FAILURES, now + LOCK_SECS).await?;
    let left = MAX_FAILURES.saturating_sub(record.failures + 1);
    let message = if left == 0 {
        format!("wrong PIN; it is now locked for {} minutes", LOCK_SECS / 60)
    } else {
        format!("wrong PIN; {left} tries left before it locks")
    };
    Err(ApiError::new(StatusCode::FORBIDDEN, "wrong_pin", message))
}

#[cfg(test)]
mod tests {
    use super::validate;

    #[test]
    fn easy_pins_are_refused() {
        for p in ["482915", "102938", "135790"] {
            assert!(validate(p).is_ok(), "{p}");
        }
        for p in ["12345", "1234567", "12a456", "000000", "123456", "654321", "777777"] {
            assert!(validate(p).is_err(), "{p}");
        }
    }
}
