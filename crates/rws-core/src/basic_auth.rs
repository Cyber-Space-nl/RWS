use crate::config::BasicAuthConfig;
use anyhow::{Context, Result, bail};
use axum::http::{HeaderMap, HeaderValue, header};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{collections::HashMap, io::Read, str::FromStr};

pub(crate) struct BasicAuth {
    users: HashMap<String, String>,
    dummy: String,
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
        Ok(Self {
            users,
            dummy,
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
        let hash = self.users.get(user);
        let valid = bcrypt::verify(password, hash.unwrap_or(&self.dummy)).unwrap_or(false);
        hash.is_some() && valid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
        assert!(!auth.verify("alice", "wrong"));
        assert!(!auth.verify("unknown", "correct:password"));
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
