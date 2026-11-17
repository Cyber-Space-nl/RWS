use crate::config::BasicAuthConfig;
use anyhow::{Context, Result, bail};
use axum::http::{HeaderMap, HeaderValue, header};
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::Read,
    str::FromStr,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};
use tokio::sync::{OnceCell, OwnedSemaphorePermit, Semaphore};

const VERIFIED_CACHE_CAPACITY: usize = 256;
const VERIFIED_CACHE_TTL: Duration = Duration::from_secs(60);
const MAX_PENDING_AUTH_REQUESTS: usize = 128;

type SharedAuthResult = OnceCell<Option<bool>>;

pub(crate) struct PendingAuth {
    pub result: Arc<SharedAuthResult>,
    _slot: OwnedSemaphorePermit,
}

pub(crate) struct BasicAuth {
    users: HashMap<String, String>,
    dummy: String,
    cache_secret: [u8; 32],
    verified: Mutex<HashMap<[u8; 32], Instant>>,
    pending: Mutex<HashMap<[u8; 32], Weak<SharedAuthResult>>>,
    pending_slots: Arc<Semaphore>,
    pub challenge: HeaderValue,
}

impl BasicAuth {
    pub fn load(config: &BasicAuthConfig) -> Result<Self> {
        if !config.htpasswd_file.is_absolute() {
            bail!("htpasswd_file must be absolute");
        }
        if config.realm.is_empty()
            || config
                .realm
                .bytes()
                .any(|b| b < 32 || b == 127 || matches!(b, b'"' | b'\\'))
        {
            bail!("invalid Basic authentication realm");
        }
        let challenge = HeaderValue::from_str(&format!(
            "Basic realm=\"{}\", charset=\"UTF-8\"",
            config.realm
        ))?;
        let mut raw = String::new();
        std::fs::File::open(&config.htpasswd_file)
            .context("cannot open htpasswd file")?
            .take(1024 * 1024 + 1)
            .read_to_string(&mut raw)
            .context("cannot read htpasswd file")?;
        if raw.len() > 1024 * 1024 {
            bail!("htpasswd file exceeds 1 MiB");
        }
        let mut users = HashMap::new();
        for line in raw
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
        {
            let (user, hash) = line.split_once(':').context("invalid htpasswd entry")?;
            if user.is_empty() || user.chars().any(char::is_control) {
                bail!("invalid htpasswd username");
            }
            if !["$2a$", "$2b$", "$2y$"]
                .iter()
                .any(|prefix| hash.starts_with(prefix))
            {
                bail!("htpasswd supports bcrypt hashes only; use htpasswd -B");
            }
            let parts = bcrypt::HashParts::from_str(hash)
                .map_err(|_| anyhow::anyhow!("invalid bcrypt hash in htpasswd file"))?;
            if !(4..=16).contains(&parts.get_cost()) {
                bail!("htpasswd bcrypt cost must be between 4 and 16");
            }
            if users.insert(user.to_owned(), hash.to_owned()).is_some() {
                bail!("duplicate htpasswd username");
            }
        }
        let dummy = users
            .values()
            .next()
            .context("htpasswd file is empty")?
            .clone();
        let mut cache_secret = [0; 32];
        rustls::crypto::aws_lc_rs::default_provider()
            .secure_random
            .fill(&mut cache_secret)
            .map_err(|_| anyhow::anyhow!("cannot initialize Basic Auth cache secret"))?;
        Ok(Self {
            users,
            dummy,
            cache_secret,
            verified: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            pending_slots: Arc::new(Semaphore::new(MAX_PENDING_AUTH_REQUESTS)),
            challenge,
        })
    }

    pub fn credentials(headers: &HeaderMap) -> Option<(String, String)> {
        if headers.get_all(header::AUTHORIZATION).iter().count() != 1 {
            return None;
        }
        let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
        if value.len() > 2048 {
            return None;
        }
        let (scheme, encoded) = value.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("basic") {
            return None;
        }
        let decoded = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
        let (user, password) = decoded.split_once(':')?;
        if user.is_empty()
            || user.chars().any(char::is_control)
            || password.len() > 72
            || password.contains('\0')
        {
            return None;
        }
        Some((user.to_owned(), password.to_owned()))
    }

    pub fn verify(&self, user: &str, password: &str) -> bool {
        if self.is_verified(user, password) {
            return true;
        }
        let hash = self.users.get(user);
        let valid = bcrypt::verify(password, hash.unwrap_or(&self.dummy)).unwrap_or(false);
        if hash.is_none() || !valid {
            return false;
        }
        let now = Instant::now();
        let mut verified = self
            .verified
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        verified.retain(|_, expires| *expires > now);
        if verified.len() >= VERIFIED_CACHE_CAPACITY
            && let Some(oldest) = verified
                .iter()
                .min_by_key(|(_, expires)| **expires)
                .map(|(key, _)| *key)
        {
            verified.remove(&oldest);
        }
        verified.insert(self.cache_key(user, password), now + VERIFIED_CACHE_TTL);
        true
    }

    pub fn is_verified(&self, user: &str, password: &str) -> bool {
        let key = self.cache_key(user, password);
        let verified = self
            .verified
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        verified
            .get(&key)
            .is_some_and(|expires| *expires > Instant::now())
    }

    pub fn pending_check(&self, user: &str, password: &str) -> Option<PendingAuth> {
        let slot = Arc::clone(&self.pending_slots).try_acquire_owned().ok()?;
        let key = self.cache_key(user, password);
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        pending.retain(|_, result| result.strong_count() > 0);
        let result = pending
            .get(&key)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let result = Arc::new(OnceCell::new());
                pending.insert(key, Arc::downgrade(&result));
                result
            });
        Some(PendingAuth {
            result,
            _slot: slot,
        })
    }

    fn cache_key(&self, user: &str, password: &str) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(self.cache_secret);
        digest.update((user.len() as u64).to_be_bytes());
        digest.update(user.as_bytes());
        digest.update(password.as_bytes());
        digest.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn pending_checks_share_work_and_release_capacity_after_cancellation() {
        let directory = tempdir().unwrap();
        let file = directory.path().join("users");
        std::fs::write(
            &file,
            format!("alice:{}\n", bcrypt::hash("password", 4).unwrap()),
        )
        .unwrap();
        let auth = BasicAuth::load(&BasicAuthConfig {
            realm: "Test".to_owned(),
            htpasswd_file: file,
        })
        .unwrap();
        let first = auth.pending_check("alice", "password").unwrap();
        let following = auth.pending_check("alice", "password").unwrap();
        assert!(Arc::ptr_eq(&first.result, &following.result));
        let wrong = auth.pending_check("alice", "wrong").unwrap();
        assert!(!Arc::ptr_eq(&first.result, &wrong.result));
        {
            let mut canceled = std::pin::pin!(first.result.get_or_init(std::future::pending));
            assert!(futures_util::poll!(&mut canceled).is_pending());
        }
        assert_eq!(
            *following.result.get_or_init(|| async { Some(true) }).await,
            Some(true)
        );
        assert_eq!(
            *wrong.result.get_or_init(|| async { Some(false) }).await,
            Some(false)
        );
        drop((first, following, wrong));
        let fresh = auth.pending_check("alice", "wrong").unwrap();
        assert!(fresh.result.get().is_none());
        drop(fresh);
        let pending: Vec<_> = (0..MAX_PENDING_AUTH_REQUESTS)
            .map(|n| auth.pending_check("alice", &n.to_string()).unwrap())
            .collect();
        assert!(auth.pending_check("alice", "overflow").is_none());
        assert_eq!(
            auth.pending.lock().unwrap().len(),
            MAX_PENDING_AUTH_REQUESTS
        );
        drop(pending);
        assert!(auth.pending_check("alice", "password").is_some());
        assert!(auth.pending.lock().unwrap().len() <= 1);
    }

    #[test]
    fn bcrypt_htpasswd_checks_credentials_and_rejects_unsupported_files() {
        let directory = tempdir().unwrap();
        let file = directory.path().join("users");
        let hash = bcrypt::hash("correct:password", 4)
            .unwrap()
            .replacen("$2b$", "$2y$", 1);
        std::fs::write(&file, format!("alice:{hash}\n")).unwrap();
        let config = BasicAuthConfig {
            realm: "Private app".to_owned(),
            htpasswd_file: file.clone(),
        };
        let auth = BasicAuth::load(&config).unwrap();
        assert!(auth.verify("alice", "correct:password"));
        assert!(auth.is_verified("alice", "correct:password"));
        assert!(!auth.is_verified("alice", "wrong"));
        assert!(!auth.is_verified("unknown", "correct:password"));
        let key = auth.cache_key("alice", "correct:password");
        auth.verified.lock().unwrap().insert(key, Instant::now());
        assert!(!auth.is_verified("alice", "correct:password"));
        assert!(auth.verify("alice", "correct:password"));
        assert!(auth.is_verified("alice", "correct:password"));
        let fresh = BasicAuth::load(&config).unwrap();
        assert!(!fresh.is_verified("alice", "correct:password"));
        assert!(!auth.verify("alice", "wrong"));
        assert!(!auth.verify("unknown", "correct:password"));
        assert_eq!(auth.verified.lock().unwrap().len(), 1);
        {
            let mut verified = auth.verified.lock().unwrap();
            for n in 0..VERIFIED_CACHE_CAPACITY {
                verified.insert(
                    auth.cache_key(&n.to_string(), "test"),
                    Instant::now() + VERIFIED_CACHE_TTL,
                );
            }
            verified.remove(&key);
        }
        assert!(auth.verify("alice", "correct:password"));
        assert_eq!(auth.verified.lock().unwrap().len(), VERIFIED_CACHE_CAPACITY);
        for entry in [
            "",
            "alice:plain-text",
            "alice:$apr1$unsupported",
            "alice:$2y$invalid",
            &format!("alice:{hash}\nalice:{hash}"),
        ] {
            std::fs::write(&file, entry).unwrap();
            assert!(BasicAuth::load(&config).is_err());
        }
        std::fs::write(&file, format!("alice:{hash}\n")).unwrap();
        let mut invalid = config;
        invalid.realm = "bad\"realm".to_owned();
        assert!(BasicAuth::load(&invalid).is_err());
    }

    #[test]
    fn basic_authorization_is_strict_and_never_accepts_ambiguous_headers() {
        let mut headers = HeaderMap::new();
        let valid = format!("bAsIc {}", STANDARD.encode("alice:correct:password"));
        headers.insert(header::AUTHORIZATION, valid.parse().unwrap());
        assert_eq!(
            BasicAuth::credentials(&headers),
            Some(("alice".to_owned(), "correct:password".to_owned()))
        );
        headers.append(header::AUTHORIZATION, valid.parse().unwrap());
        assert!(BasicAuth::credentials(&headers).is_none());
        for value in [
            "Bearer token".to_owned(),
            "Basic !!!".to_owned(),
            format!("Basic {}", STANDARD.encode("no-colon")),
            format!(
                "Basic {}",
                STANDARD.encode(format!("alice:{}", "x".repeat(73)))
            ),
            format!("Basic {}", STANDARD.encode("alice:bad\0password")),
        ] {
            headers.clear();
            headers.insert(header::AUTHORIZATION, value.parse().unwrap());
            assert!(BasicAuth::credentials(&headers).is_none());
        }
    }
}
