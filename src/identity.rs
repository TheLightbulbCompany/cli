//! This computer's member identity: the Ed25519 key, and the signatures every signed object
//! carries. On macOS the key is a Keychain item (`KEYCHAIN_SERVICE`); on Linux it is the 0600
//! file `identity.key`.

use crate::Result;
use data_encoding::{Encoding, Specification, BASE64URL_NOPAD};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
#[cfg(target_os = "macos")]
use security_framework::os::macos::keychain::SecKeychain;
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
    /// Loads this computer's key (a raw 32-byte Ed25519 secret), creating it on first run: in
    /// the Keychain on macOS (`keychain`), else the 0600 file `identity.key`. The caller holds
    /// `daemon.lock`.
    pub fn load_or_create(dir: &Path) -> Result<Identity> {
        #[cfg(target_os = "macos")]
        return Identity::load_or_create_in(dir, keychain()?.as_ref());
        #[cfg(not(target_os = "macos"))]
        Ok(Identity::of(file_key(dir, true)?.expect("created")))
    }

    /// `load_or_create` with the key in `kc` (`None`: the file). An `identity.key` left by an
    /// earlier version moves into `kc` once: written, read back, compared, then deleted. A
    /// Keychain that cannot be read is an error, never a reason to use the file.
    #[cfg(target_os = "macos")]
    #[doc(hidden)]
    pub fn load_or_create_in(dir: &Path, kc: Option<&SecKeychain>) -> Result<Identity> {
        let Some(kc) = kc else {
            return Ok(Identity::of(file_key(dir, true)?.expect("created")));
        };
        let account = account(dir)?;
        let path = dir.join("identity.key");
        let file = file_key(dir, false)?;
        let signing = match item(kc, &account)? {
            Some(k) => {
                // A move that stopped before its delete: finish it, if the two agree.
                if let Some(f) = file {
                    if f.to_bytes() != k.to_bytes() {
                        return Err(
                            "identity.key and the Keychain hold different keys; nothing changed."
                                .into(),
                        );
                    }
                    fs::remove_file(&path)?;
                }
                k
            }
            None => {
                let key = file.unwrap_or_else(|| {
                    SigningKey::generate(&mut rand::rand_core::UnwrapErr(rand::rngs::SysRng))
                });
                kc.add_generic_password(KEYCHAIN_SERVICE, &account, &key.to_bytes())
                    .map_err(keychain_error)?;
                if item(kc, &account)?.map(|k| k.to_bytes()) != Some(key.to_bytes()) {
                    return Err("The Keychain did not keep this computer's key.".into());
                }
                let _ = fs::remove_file(&path);
                key
            }
        };
        Ok(Identity::of(signing))
    }

    fn of(signing: SigningKey) -> Identity {
        let key = key_of(signing.verifying_key().as_bytes());
        Identity {
            short: key[..12].to_string(),
            key,
            signing,
        }
    }

    /// The `sig` for `v`: Ed25519, base64url, over `canonical(v without sig)`.
    pub fn sign(&self, v: &Value) -> String {
        BASE64URL_NOPAD.encode(&self.signing.sign(&unsigned(v)).to_bytes())
    }

    /// `v` with its `sig` (`sign`).
    pub fn signed(&self, mut v: Value) -> Value {
        v["sig"] = self.sign(&v).into();
        v
    }
}

/// Whether this computer has a key yet (without making one).
pub fn exists(dir: &Path) -> Result<bool> {
    #[cfg(target_os = "macos")]
    return exists_in(dir, keychain()?.as_ref());
    #[cfg(not(target_os = "macos"))]
    Ok(dir.join("identity.key").exists())
}

/// `exists` with the key in `kc` (`None`: the file).
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub fn exists_in(dir: &Path, kc: Option<&SecKeychain>) -> Result<bool> {
    let file = dir.join("identity.key").exists();
    match kc {
        Some(kc) => Ok(file || item(kc, &account(dir)?)?.is_some()),
        None => Ok(file),
    }
}

/// `identity.key`, when there is one; with `create`, made on first run (write then rename, so a
/// crash never leaves a truncated key; we hold the lock, so a leftover .tmp is ours to replace).
fn file_key(dir: &Path, create: bool) -> Result<Option<SigningKey>> {
    let path = dir.join("identity.key");
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(SigningKey::from_bytes(
            &bytes
                .try_into()
                .map_err(|_| "identity.key is not 32 bytes")?,
        ))),
        Err(e) if e.kind() == ErrorKind::NotFound && create => {
            let tmp = dir.join("identity.key.tmp");
            let key = SigningKey::generate(&mut rand::rand_core::UnwrapErr(rand::rngs::SysRng));
            crate::setup::create_with(&tmp, &key.to_bytes(), 0o600)?;
            fs::rename(&tmp, &path)?;
            Ok(Some(key))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The Keychain item's service. Its account is the state dir's canonical path, so two state
/// dirs (and the tests' temp dirs) never share a key.
#[cfg(target_os = "macos")]
pub const KEYCHAIN_SERVICE: &str = "lightbulb-identity";

/// The keychain the key lives in: the login (default) keychain. A file-based keychain item
/// stays on this Mac (iCloud Keychain syncs only the data-protection keychain, which an
/// unentitled command-line binary cannot use), and its access list trusts the binary that
/// made it (our Developer ID signature, so an update still reads it).
/// ponytail: debug builds keep the file, so `cargo test` never writes the login keychain; the
/// Keychain path is tested against a throwaway keychain (`load_or_create_in`).
#[cfg(target_os = "macos")]
fn keychain() -> Result<Option<SecKeychain>> {
    if cfg!(debug_assertions) {
        return Ok(None);
    }
    Ok(Some(SecKeychain::default().map_err(keychain_error)?))
}

#[cfg(target_os = "macos")]
fn account(dir: &Path) -> Result<String> {
    Ok(dir.canonicalize()?.to_string_lossy().into_owned())
}

/// The key in `kc` for `account`; `None` when there is no item.
#[cfg(target_os = "macos")]
fn item(kc: &SecKeychain, account: &str) -> Result<Option<SigningKey>> {
    /// errSecItemNotFound.
    const NOT_FOUND: i32 = -25300;
    match kc.find_generic_password(KEYCHAIN_SERVICE, account) {
        Ok((pw, _)) => Ok(Some(SigningKey::from_bytes(
            pw.as_ref()
                .try_into()
                .map_err(|_| "The Keychain's lightbulb-identity item is not 32 bytes.")?,
        ))),
        Err(e) if e.code() == NOT_FOUND => Ok(None),
        Err(e) => Err(keychain_error(e).into()),
    }
}

/// A Keychain failure, for the person: the daemon refuses to start on it.
#[cfg(target_os = "macos")]
fn keychain_error(e: security_framework::base::Error) -> String {
    format!(
        "Could not use the Keychain for this computer's key (error {}). Unlock your login keychain, then run: lightbulb doctor",
        e.code()
    )
}

/// The member key of the 32 public-key bytes `pk`.
pub fn key_of(pk: &[u8; 32]) -> String {
    BASE32.encode(pk)
}

/// The 32 public-key bytes member key `key` spells, if it is one.
pub fn key_bytes(key: &str) -> Option<[u8; 32]> {
    BASE32.decode(key.as_bytes()).ok()?.try_into().ok()
}

/// Whether `sig` is `key`'s signature over `v` (its own `sig` field left out).
pub fn verify(key: &str, v: &Value, sig: &str) -> bool {
    // `BASE32` is lowercase only: an uppercase spelling is another (invalid) key.
    let key = key_bytes(key).and_then(|k| VerifyingKey::from_bytes(&k).ok());
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
