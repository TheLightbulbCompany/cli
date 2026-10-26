//! The tests' issuer: the private half of debug builds' `proof::TEST_ISSUER`, and proofs
//! minted with it. Also compiled into `proof.rs`'s unit tests (`#[path]`). Never in a binary.
#![allow(dead_code)]

use data_encoding::BASE64URL_NOPAD;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};

pub const KID: &str = "lightbulb-debug-test-issuer";
const SECRET: [u8; 32] = [
    245, 60, 96, 180, 211, 142, 178, 235, 238, 225, 93, 251, 197, 204, 6, 143, 58, 16, 28, 202,
    170, 201, 236, 201, 55, 216, 224, 92, 132, 156, 249, 237,
];

/// A compact EdDSA JWS of `claims`, signed by the test issuer.
pub fn mint(claims: &Value) -> String {
    let b64 = |v: &Value| BASE64URL_NOPAD.encode(v.to_string().as_bytes());
    let signing = format!(
        "{}.{}",
        b64(&json!({"alg": "EdDSA", "kid": KID})),
        b64(claims)
    );
    let sig = SigningKey::from_bytes(&SECRET).sign(signing.as_bytes());
    format!("{signing}.{}", BASE64URL_NOPAD.encode(&sig.to_bytes()))
}

/// A sign-in proof: computer `computer` belongs to Lightbulb user `sub`, from `iat`, 30 days.
pub fn proof(computer: &str, sub: &str, iat: i64) -> String {
    mint(&json!({"iat": iat, "exp": iat + 30 * 86_400, "sub": sub, "computer": computer}))
}
