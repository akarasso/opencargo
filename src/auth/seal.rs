//! AEAD-sealed values for cookies that must carry state the server does not
//! keep: AES-256-GCM under a server secret, the purpose bound as associated
//! data, the expiry inside the ciphertext.

use aws_lc_rs::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use base64::Engine;
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};

pub const KEY_LEN: usize = 32;

pub struct Sealer {
    key: LessSafeKey,
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    exp: i64,
    body: serde_json::Value,
}

impl Sealer {
    pub fn new(key: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(key.len() == KEY_LEN, "a sealing key is {KEY_LEN} bytes");
        let unbound = UnboundKey::new(&AES_256_GCM, key)
            .map_err(|_| anyhow::anyhow!("invalid sealing key"))?;
        Ok(Self {
            key: LessSafeKey::new(unbound),
        })
    }

    pub fn random_key() -> Vec<u8> {
        let mut key = vec![0u8; KEY_LEN];
        rand::thread_rng().fill_bytes(&mut key);
        key
    }

    pub fn seal<T: Serialize>(
        &self,
        purpose: &str,
        value: &T,
        expires_at: DateTime<Utc>,
    ) -> String {
        let envelope = Envelope {
            exp: expires_at.timestamp(),
            body: serde_json::to_value(value).expect("a sealed value serializes"),
        };
        let mut data = serde_json::to_vec(&envelope).expect("an envelope serializes");
        let mut nonce = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce);
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(purpose.as_bytes()),
                &mut data,
            )
            .expect("sealing never fails on a valid key");
        let mut out = nonce.to_vec();
        out.extend_from_slice(&data);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(out)
    }

    /// `None` for anything forged, tampered with, sealed for another purpose
    /// or expired.
    pub fn open<T: for<'de> Deserialize<'de>>(
        &self,
        purpose: &str,
        sealed: &str,
        now: DateTime<Utc>,
    ) -> Option<T> {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(sealed)
            .ok()?;
        if raw.len() < NONCE_LEN {
            return None;
        }
        let (nonce, rest) = raw.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
        let mut data = rest.to_vec();
        let plain = self
            .key
            .open_in_place(nonce, Aad::from(purpose.as_bytes()), &mut data)
            .ok()?;
        let envelope: Envelope = serde_json::from_slice(plain).ok()?;
        if envelope.exp < now.timestamp() {
            return None;
        }
        serde_json::from_value(envelope.body).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn a_sealed_value_opens_only_for_its_purpose_key_and_lifetime() {
        let now = Utc::now();
        let sealer = Sealer::new(&Sealer::random_key()).unwrap();
        let sealed = sealer.seal("login", &"hello", now + Duration::minutes(5));
        assert_eq!(
            sealer.open::<String>("login", &sealed, now).as_deref(),
            Some("hello")
        );
        assert_eq!(sealer.open::<String>("link", &sealed, now), None);
        assert_eq!(
            sealer.open::<String>("login", &sealed, now + Duration::minutes(6)),
            None
        );
        let other = Sealer::new(&Sealer::random_key()).unwrap();
        assert_eq!(other.open::<String>("login", &sealed, now), None);
        let mut tampered = sealed.into_bytes();
        let last = tampered.len() - 2;
        tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert_eq!(sealer.open::<String>("login", &tampered, now), None);
    }
}
