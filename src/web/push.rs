//! Web Push to paired phones. Pushes carry no payload, so nothing needs
//! encrypting: the phone's service worker wakes up, asks `/api/needs` what
//! happened and shows the notification itself. Each push is signed with
//! this machine's VAPID key (RFC 8292), made once and kept in
//! `~/.corral/web/vapid.key`.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::{Signature, SigningKey, signature::Signer};

use super::auth::{b64, random, write_private};

/// Push services want a contact for whoever sends; Apple refuses pushes
/// without one.
const SUBJECT: &str = "https://github.com/bottercode/corral";

#[derive(Clone)]
pub struct Vapid {
    key: SigningKey,
}

impl Vapid {
    pub fn load_or_create(dir: &Path) -> Result<Vapid> {
        let path = dir.join("vapid.key");
        if let Ok(s) = std::fs::read_to_string(&path) {
            let bytes = URL_SAFE_NO_PAD
                .decode(s.trim())
                .with_context(|| format!("reading {}", path.display()))?;
            let key = SigningKey::from_slice(&bytes)
                .with_context(|| format!("reading {}", path.display()))?;
            return Ok(Vapid { key });
        }
        // Almost every 32 random bytes are a valid key; try again if not.
        let key = loop {
            if let Ok(k) = SigningKey::from_slice(&random::<32>()?) {
                break k;
            }
        };
        write_private(&path, b64(&key.to_bytes()).as_bytes())?;
        Ok(Vapid { key })
    }

    /// The public key, as the browser's `applicationServerKey` takes it.
    pub fn public_key(&self) -> String {
        b64(self.key.verifying_key().to_encoded_point(false).as_bytes())
    }

    /// A signed token for the push service behind `endpoint`.
    pub fn jwt(&self, endpoint: &str, now: SystemTime) -> Result<String> {
        let aud = audience(endpoint)?;
        let exp = now.duration_since(UNIX_EPOCH)?.as_secs() + 12 * 3600;
        let header = b64(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = b64(
            serde_json::json!({ "aud": aud, "exp": exp, "sub": SUBJECT })
                .to_string()
                .as_bytes(),
        );
        let input = format!("{header}.{claims}");
        let sig: Signature = self.key.sign(input.as_bytes());
        Ok(format!("{input}.{}", b64(&sig.to_bytes())))
    }

    /// Wake the phone behind `endpoint`. `Ok(false)`: the subscription is
    /// gone (the phone unsubscribed or was reset) and can be forgotten.
    pub fn send(&self, endpoint: &str) -> Result<bool> {
        let jwt = self.jwt(endpoint, SystemTime::now())?;
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .build()
            .new_agent();
        let result = agent
            .post(endpoint)
            .header("TTL", "3600")
            .header("Urgency", "high")
            .header(
                "Authorization",
                format!("vapid t={jwt}, k={}", self.public_key()),
            )
            .send_empty();
        match result {
            Ok(_) => Ok(true),
            Err(ureq::Error::StatusCode(404 | 410)) => Ok(false),
            Err(e) => Err(e).context("sending a push"),
        }
    }
}

/// `https://host[:port]` of a push endpoint.
pub fn audience(endpoint: &str) -> Result<String> {
    let Some(rest) = endpoint.strip_prefix("https://") else {
        bail!("push endpoints must be https");
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() || host.contains('@') {
        bail!("not a push endpoint: {endpoint}");
    }
    Ok(format!("https://{host}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{VerifyingKey, signature::Verifier};

    #[test]
    fn keys_persist() {
        let dir = tempfile::tempdir().unwrap();
        let a = Vapid::load_or_create(dir.path()).unwrap();
        let b = Vapid::load_or_create(dir.path()).unwrap();
        assert_eq!(a.public_key(), b.public_key());
        // Uncompressed P-256 point: 65 bytes.
        assert_eq!(URL_SAFE_NO_PAD.decode(a.public_key()).unwrap().len(), 65);
    }

    #[test]
    fn jwt_is_signed_for_the_push_service() {
        let dir = tempfile::tempdir().unwrap();
        let v = Vapid::load_or_create(dir.path()).unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let jwt = v
            .jwt("https://web.push.apple.com/QGx1c3Q?x=1", now)
            .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://web.push.apple.com");
        assert_eq!(claims["exp"], 1_000_000 + 12 * 3600);
        assert_eq!(claims["sub"], SUBJECT);

        let point = URL_SAFE_NO_PAD.decode(v.public_key()).unwrap();
        let key = VerifyingKey::from_sec1_bytes(&point).unwrap();
        let sig = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
        key.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .unwrap();
    }

    #[test]
    fn audiences() {
        assert_eq!(
            audience("https://fcm.googleapis.com/fcm/send/abc").unwrap(),
            "https://fcm.googleapis.com"
        );
        assert!(audience("http://fcm.googleapis.com/x").is_err());
        assert!(audience("https:///x").is_err());
        assert!(audience("https://user@host/x").is_err());
    }
}
