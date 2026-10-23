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

/// A computer proof for `computer`: person `sub` called `name`, signed into workspace `ws`
/// (`(id, name)`), issued at `iat`, valid a day.
pub fn computer_proof(
    computer: &str,
    sub: &str,
    name: &str,
    ws: Option<(&str, &str)>,
    iat: i64,
) -> String {
    mint(
        &json!({"iss": "test", "iat": iat, "exp": iat + 86_400, "sub": sub, "name": name,
        "computer": computer, "kind": "computer",
        "workspace": ws.map(|(id, name)| json!({"id": id, "name": name})), "agents": []}),
    )
}

/// A workspace proof for `computer`: workspace `id` called `name` with `agents`, issued at
/// `iat`, valid a day.
pub fn workspace_proof(computer: &str, id: &str, name: &str, agents: &[&str], iat: i64) -> String {
    mint(
        &json!({"iss": "test", "iat": iat, "exp": iat + 86_400, "sub": format!("ws:{id}"),
        "name": name, "computer": computer, "kind": "workspace",
        "workspace": {"id": id, "name": name}, "agents": agents}),
    )
}
