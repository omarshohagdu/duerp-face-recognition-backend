//! Tokens this service issues from `POST /login` and validates on
//! enroll/verify.
//!
//! `JWT_SECRET` must be the SAME value duerp-api uses. Both services mint and
//! accept tokens with this shape, so a token obtained from either one works on
//! the other — that is what makes the split invisible to existing clients.
//! Rotating the secret is therefore a two-service operation.

use jsonwebtoken::{encode, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::env;

/// `sub` is the *person id* the attendance flows key off: the employee id for
/// staff, the DU `user_id` for students (see `routes::auth::login`).
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: u64,
    pub exp: usize,
}

pub fn create_jwt(user_id: u64) -> String {
    let secret = env::var("JWT_SECRET").unwrap();
    let exp = chrono::Utc::now()
        .checked_add_signed(chrono::Duration::hours(730))
        .unwrap()
        .timestamp() as usize;

    let claims = Claims { sub: user_id, exp };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_ref()),
    )
    .unwrap()
}