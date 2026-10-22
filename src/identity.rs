//! This computer's member identity: the Ed25519 key in `identity.key`, and the signatures
//! every signed object carries.

use crate::Result;
use data_encoding::{Encoding, Specification, BASE64URL_NOPAD};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde_json::Value;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::LazyLock;

/// The member key's alphabet: base32, lowercase, no padding.
static BASE32: LazyLock<Encoding> = LazyLock::new(|| {
    let mut s = Specification::new();
    s.symbols.push_str("abcdefghijklmnopqrstuvwxyz234567");
    s.encoding().expect("a valid base32 alphabet")
});

pub struct Identity {
    pub signing: SigningKey,
    /// The public key, lowercase base32 without padding: the member key.
    pub key: String,
    /// `key`'s first 12 characters.
    pub short: String,
}

impl Identity {
    /// Loads `identity.key` (a raw 32-byte Ed25519 secret), creating it 0600 on first run.
    /// The caller holds `daemon.lock`.
    pub fn load_or_create(dir: &Path) -> Result<Identity> {
        let path = dir.join("identity.key");
        let signing = match fs::read(&path) {
            Ok(bytes) => SigningKey::from_bytes(
                &bytes
                    .try_into()
                    .map_err(|_| "identity.key is not 32 bytes")?,
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                // Write then rename, so a crash never leaves a truncated key.
                // We hold the lock, so a leftover .tmp is ours to replace.
                let tmp = dir.join("identity.key.tmp");
                let key = SigningKey::generate(&mut rand::rngs::OsRng);
                crate::setup::create_with(&tmp, &key.to_bytes(), 0o600)?;
                fs::rename(&tmp, &path)?;
                key
            }
            Err(e) => return Err(e.into()),
        };
        let key = BASE32.encode(signing.verifying_key().as_bytes());
        Ok(Identity {
            short: key[..12].to_string(),
            key,
            signing,
        })
    }

    /// The `sig` for `v`: Ed25519, base64url, over `canonical(v without sig)`.
    pub fn sign(&self, v: &Value) -> String {
        BASE64URL_NOPAD.encode(&self.signing.sign(&unsigned(v)).to_bytes())
    }
}

/// Whether `sig` is `key`'s signature over `v` (its own `sig` field left out).
pub fn verify(key: &str, v: &Value, sig: &str) -> bool {
    // `BASE32` is lowercase only: an uppercase spelling is another (invalid) key.
    let key = BASE32
        .decode(key.as_bytes())
        .ok()
        .and_then(|k| VerifyingKey::from_bytes(&k.try_into().ok()?).ok());
    let sig = BASE64URL_NOPAD
        .decode(sig.as_bytes())
        .ok()
        .and_then(|s| Signature::from_slice(&s).ok());
    match (key, sig) {
        (Some(k), Some(s)) => k.verify_strict(&unsigned(v), &s).is_ok(),
        _ => false,
    }
}

/// `canonical(v)` with its `sig` field left out.
pub(crate) fn unsigned(v: &Value) -> Vec<u8> {
    match v {
        Value::Object(m) if m.contains_key("sig") => {
            let mut m = m.clone();
            m.remove("sig");
            canonical(&Value::Object(m))
        }
        _ => canonical(v),
    }
}

/// `v` as JSON with object keys sorted recursively and no whitespace. Sorted here, never
/// left to serde_json's map order (a dependency may enable `preserve_order`).
pub fn canonical(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(&Value::String(k.clone()), out);
                out.push(b':');
                write_canonical(&m[k], out);
            }
            out.push(b'}');
        }
        Value::Array(a) => {
            out.push(b'[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(x, out);
            }
            out.push(b']');
        }
        // Scalars have one compact form.
        _ => out.extend_from_slice(v.to_string().as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_sorts_keys_and_drops_whitespace() {
        assert_eq!(
            canonical(&json!({"b":1,"a":{"d":2,"c":3}})),
            br#"{"a":{"c":3,"d":2},"b":1}"#
        );
    }

    #[test]
    fn sign_and_verify_ignore_the_sig_field() {
        let dir = tempfile::tempdir().unwrap();
        let me = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(Identity::load_or_create(dir.path()).unwrap().key, me.key);
        assert_eq!(me.short, me.key[..12]);
        let mut v = json!({"t": "x", "n": [1, "é\""]});
        let sig = me.sign(&v);
        v["sig"] = sig.clone().into();
        assert!(verify(&me.key, &v, &sig));
        v["n"] = json!([2]);
        assert!(!verify(&me.key, &v, &sig));
        assert!(!verify("not a key", &v, &sig));
        v["n"] = json!([1, "é\""]);
        assert!(verify(&me.key, &v, &sig));
        assert!(!verify(&me.key.to_uppercase(), &v, &sig));
    }
}
