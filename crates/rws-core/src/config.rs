use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    collections::HashSet,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub static_cache: StaticCacheConfig,
    #[serde(default)]
    pub websocket: WebSocketConfig,
    pub tls: Option<TlsConfig>,
    #[serde(default)]
    pub adaptive: AdaptiveConfig,
    pub sites: Vec<SiteConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticCacheConfig {
    pub enabled: bool,
    pub max_total_bytes: usize,
    pub max_file_bytes: usize,
    pub max_entries: usize,
    pub revalidate_after_ms: u64,
}

impl Default for StaticCacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_total_bytes: 64 * 1024 * 1024,
            max_file_bytes: 1024 * 1024,
            max_entries: 4096,
            revalidate_after_ms: 1000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub https_listen: SocketAddr,
    /// Redirect HTTP to the HTTPS listener when TLS is configured.
    pub redirect_http_to_https: bool,
    pub max_in_flight_requests: usize,
    pub listen_backlog: u32,
    pub max_body_bytes: usize,
    pub max_proxy_response_bytes: usize,
    pub max_fastcgi_response_bytes: usize,
    pub request_timeout_ms: u64,
    pub shutdown_grace_period_s: u64,
    pub json_logs: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".parse().expect("valid default address"),
            https_listen: "0.0.0.0:8443".parse().expect("valid default address"),
            redirect_http_to_https: true,
            max_in_flight_requests: 1024,
            listen_backlog: 1024,
            max_body_bytes: 8 * 1024 * 1024,
            max_proxy_response_bytes: 8 * 1024 * 1024,
            max_fastcgi_response_bytes: 8 * 1024 * 1024,
            request_timeout_ms: 30_000,
            shutdown_grace_period_s: 15,
            json_logs: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    pub listen: SocketAddr,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:9091".parse().expect("valid default address"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSocketConfig {
    pub path: String,
    pub outbound_queue_messages: usize,
    pub outbound_queue_bytes: usize,
    pub scheduler_quantum_bytes: usize,
    pub per_turn_messages: usize,
}

impl Default for WebSocketConfig {
    fn default() -> Self {
        Self {
            path: "/_rws/ws".to_owned(),
            outbound_queue_messages: 64,
            outbound_queue_bytes: 256 * 1024,
            scheduler_quantum_bytes: 16 * 1024,
            per_turn_messages: 16,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdaptiveConfig {
    pub enabled: bool,
    pub interval_ms: u64,
    pub min_quantum_bytes: usize,
    pub max_quantum_bytes: usize,
    pub queue_high_watermark: usize,
}

impl Default for AdaptiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_ms: 500,
            min_quantum_bytes: 4 * 1024,
            max_quantum_bytes: 128 * 1024,
            queue_high_watermark: 128,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SiteMode {
    Static,
    Proxy,
}

fn default_mode() -> SiteMode {
    SiteMode::Static
}

fn default_index() -> String {
    "index.html".to_owned()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteConfig {
    pub host: String,
    #[serde(default = "default_mode")]
    pub mode: SiteMode,
    pub root: Option<PathBuf>,
    #[serde(default = "default_index")]
    pub index: String,
    #[serde(default)]
    pub php: bool,
    /// Serve generated directory indexes only when explicitly enabled.
    #[serde(default)]
    pub directory_listing: bool,
    /// Compress eligible static responses when the client accepts gzip.
    #[serde(default)]
    pub gzip: bool,
    pub php_fpm: Option<String>,
    pub front_controller: Option<String>,
    pub access_log: Option<PathBuf>,
    pub error_log: Option<PathBuf>,
    pub upstream: Option<String>,
    #[serde(default)]
    pub locations: Vec<LocationConfig>,
}

/// A separate static/PHP application mounted below a site URL prefix.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocationConfig {
    pub path: String,
    pub root: PathBuf,
    #[serde(default = "default_index")]
    pub index: String,
    #[serde(default)]
    pub php: bool,
    pub php_fpm: Option<String>,
    pub front_controller: Option<String>,
    #[serde(default)]
    pub gzip: bool,
    #[serde(default)]
    pub deny_paths: Vec<String>,
    #[serde(default)]
    pub deny_files: Vec<String>,
    #[serde(default)]
    pub noindex: bool,
    pub basic_auth: Option<BasicAuthConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BasicAuthConfig {
    pub realm: String,
    pub htpasswd_file: PathBuf,
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read configuration {}", path.display()))?;
        let config: Self = toml::from_str(&raw)
            .with_context(|| format!("invalid configuration {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.sites.is_empty() {
            bail!("at least one [[sites]] entry is required");
        }
        // A protected mount inside another public root could be reached through
        // that root's URL without its authentication or deny rules.
        let mut public_roots = Vec::new();
        for (site_id, site) in self.sites.iter().enumerate() {
            if site.mode == SiteMode::Static
                && let Some(root) = &site.root
            {
                public_roots.push((
                    site_id,
                    None,
                    root.canonicalize().context("cannot resolve site root")?,
                ));
            }
            for (location_id, location) in site.locations.iter().enumerate() {
                public_roots.push((
                    site_id,
                    Some(location_id),
                    location
                        .root
                        .canonicalize()
                        .context("cannot resolve location root")?,
                ));
            }
        }
        for (site_id, location_id, root) in &public_roots {
            if location_id.is_some()
                && public_roots
                    .iter()
                    .any(|(other_site, other_location, other)| {
                        (site_id, location_id) != (other_site, other_location)
                            && (root.starts_with(other) || other.starts_with(root))
                    })
            {
                bail!("location roots must not overlap other configured public roots");
            }
        }
        if self.server.max_in_flight_requests == 0
            || self.server.listen_backlog == 0
            || self.server.max_body_bytes == 0
            || self.server.max_proxy_response_bytes == 0
            || self.server.max_fastcgi_response_bytes == 0
            || self.server.request_timeout_ms == 0
            || self.server.shutdown_grace_period_s == 0
            || self.websocket.outbound_queue_messages == 0
            || self.websocket.outbound_queue_bytes == 0
            || self.websocket.scheduler_quantum_bytes == 0
            || self.websocket.per_turn_messages == 0
        {
            bail!("server and WebSocket limits must be greater than zero");
        }
        if self.static_cache.enabled
            && (self.static_cache.max_total_bytes == 0
                || self.static_cache.max_file_bytes == 0
                || self.static_cache.max_entries == 0)
        {
            bail!("enabled static cache limits must be greater than zero");
        }
        if self.static_cache.enabled
            && self.static_cache.max_file_bytes > self.static_cache.max_total_bytes
        {
            bail!("static_cache.max_file_bytes must not exceed max_total_bytes");
        }
        if !self.websocket.path.starts_with('/') {
            bail!("websocket.path must start with '/'");
        }
        let mut hosts = HashSet::new();
        for site in &self.sites {
            let host = site.host.trim().trim_end_matches('.').to_ascii_lowercase();
            if host.is_empty() || host.contains('/') || host.contains(':') {
                bail!("invalid site host '{}'", site.host);
            }
            if !hosts.insert(host) {
                bail!("duplicate site host '{}'", site.host);
            }
            let mut prefixes: Vec<&str> = Vec::new();
            for location in &site.locations {
                if site.mode != SiteMode::Static {
                    bail!("locations require a static/PHP site");
                }
                let prefix = location.path.trim_end_matches('/');
                if prefix.is_empty()
                    || !location.path.starts_with('/')
                    || !location.path.ends_with('/')
                    || location.path.contains("//")
                    || !location
                        .path
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"/-_.~".contains(&b))
                    || location
                        .path
                        .split('/')
                        .any(|part| matches!(part, "." | ".."))
                {
                    bail!("location path must be a canonical URL prefix such as '/database/'");
                }
                if prefixes.iter().any(|other| {
                    crate::location::matches_prefix(prefix, other)
                        || crate::location::matches_prefix(other, prefix)
                }) {
                    bail!("duplicate or overlapping location prefixes are not supported");
                }
                prefixes.push(prefix);
                if crate::location::matches_prefix(&self.websocket.path, prefix)
                    || crate::location::matches_prefix("/.well-known/acme-challenge", prefix)
                    || crate::location::matches_prefix(prefix, "/.well-known/acme-challenge")
                {
                    bail!("location conflicts with the native WebSocket or ACME path");
                }
                if !location.root.is_absolute() || !location.root.is_dir() {
                    bail!("location root must be an existing absolute directory");
                }
                if location.php && location.php_fpm.as_deref().is_none_or(str::is_empty) {
                    bail!("PHP location requires its own php_fpm endpoint");
                }
                let index = Path::new(&location.index);
                if location.index.is_empty()
                    || index.components().count() != 1
                    || !matches!(index.components().next(), Some(Component::Normal(_)))
                {
                    bail!("location index must be a single filename");
                }
                if let Some(front) = &location.front_controller
                    && (!location.php
                        || !crate::location::safe_relative_file(front)
                        || !front.ends_with(".php")
                        || !location.root.join(front).is_file())
                {
                    bail!("location front_controller must be an existing contained PHP script");
                }
                for path in &location.deny_paths {
                    if !path.starts_with('/')
                        || path.ends_with('/')
                        || !crate::location::safe_relative_file(path.trim_start_matches('/'))
                        || path.contains('%')
                        || path.contains("//")
                    {
                        bail!("deny_paths entries must be relative URL prefixes such as '/vendor'");
                    }
                }
                for pattern in &location.deny_files {
                    if pattern.is_empty()
                        || pattern.contains(['/', '\\', '\0'])
                        || pattern.matches('*').count() > 1
                    {
                        bail!("deny_files entries are basenames with at most one '*' wildcard");
                    }
                }
                if let Some(auth) = &location.basic_auth {
                    if self.tls.is_none() {
                        bail!("Basic authentication requires TLS");
                    }
                    let password_path = auth
                        .htpasswd_file
                        .canonicalize()
                        .context("cannot resolve htpasswd file")?;
                    if public_roots
                        .iter()
                        .any(|(_, _, root)| password_path.starts_with(root))
                    {
                        bail!("htpasswd file must be outside public roots");
                    }
                    crate::basic_auth::BasicAuth::load(auth)?;
                }
            }
            match site.mode {
                SiteMode::Static => {
                    let root = site.root.as_ref().context("static site requires root")?;
                    if !root.is_dir() {
                        bail!(
                            "site root does not exist or is not a directory: {}",
                            root.display()
                        );
                    }
                    if site.php && site.php_fpm.as_deref().unwrap_or_default().is_empty() {
                        bail!("PHP site '{}' requires php_fpm", site.host);
                    }
                    if let Some(front_controller) = &site.front_controller {
                        let path = Path::new(front_controller);
                        if !site.php {
                            bail!("front controller for '{}' requires PHP", site.host);
                        }
                        if path.is_absolute()
                            || path
                                .components()
                                .any(|component| !matches!(component, Component::Normal(_)))
                            || path.extension().and_then(|value| value.to_str()) != Some("php")
                        {
                            bail!(
                                "invalid front controller '{}' for '{}'",
                                front_controller,
                                site.host
                            );
                        }
                        if !root.join(path).is_file() {
                            bail!(
                                "front controller '{}' for '{}' does not exist",
                                front_controller,
                                site.host
                            );
                        }
                    }
                }
                SiteMode::Proxy => {
                    let upstream = site
                        .upstream
                        .as_deref()
                        .context("proxy site requires upstream")?;
                    let parsed = reqwest::Url::parse(upstream)
                        .with_context(|| format!("invalid upstream for '{}'", site.host))?;
                    if !matches!(parsed.scheme(), "http" | "https") {
                        bail!("proxy upstream must use http or https");
                    }
                }
            }
            for (kind, path) in [
                ("access log", site.access_log.as_ref()),
                ("error log", site.error_log.as_ref()),
            ] {
                if path.is_some_and(|path| !path.is_absolute()) {
                    bail!("{kind} path for '{}' must be absolute", site.host);
                }
            }
        }
        if let Some(tls) = &self.tls
            && (!tls.cert_file.is_file() || !tls.key_file.is_file())
        {
            bail!("TLS certificate or key file does not exist");
        }
        if self.adaptive.enabled
            && (self.adaptive.interval_ms < 100
                || self.adaptive.min_quantum_bytes == 0
                || self.adaptive.min_quantum_bytes > self.adaptive.max_quantum_bytes)
        {
            bail!("invalid adaptive controller limits");
        }
        Ok(())
    }

    pub fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.server.request_timeout_ms)
    }

    pub async fn validate_tls(&self) -> Result<()> {
        if let Some(tls) = &self.tls {
            crate::tls::ensure_crypto_provider();
            axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert_file, &tls.key_file)
                .await
                .context("failed to load TLS certificate/key")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn rejects_unknown_fields() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("index.html"), "ok").unwrap();
        let text = format!(
            "[server]\nunknown = true\n[[sites]]\nhost='example.test'\nroot='{}'\n",
            dir.path().display()
        );
        assert!(toml::from_str::<Config>(&text).is_err());
    }

    #[test]
    fn validates_static_site() {
        let dir = tempdir().unwrap();
        let text = format!(
            "[[sites]]\nhost='example.test'\nroot='{}'\n",
            dir.path().display()
        );
        let config: Config = toml::from_str(&text).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn validates_location_boundaries_and_rejects_ambiguous_or_unsafe_configuration() {
        let dir = tempdir().unwrap();
        let public = dir.path().join("public");
        std::fs::create_dir(&public).unwrap();
        let application = dir.path().join("application");
        std::fs::create_dir(&application).unwrap();
        let base = format!(
            "[[sites]]\nhost='example.test'\nroot='{}'\n",
            public.display()
        );
        let valid = format!(
            "[[sites.locations]]\npath='/database/'\nroot='{}'\ndeny_paths=['/vendor']\ndeny_files=['config*.php']\n",
            application.display()
        );
        let config: Config = toml::from_str(&format!("{base}{valid}")).unwrap();
        config.validate().unwrap();
        let mut overlapping = config.clone();
        overlapping.sites[0].locations[0].root = public.clone();
        assert!(overlapping.validate().is_err());
        let another = dir.path().join("another");
        std::fs::create_dir(&another).unwrap();
        for prefix in [
            "/",
            "/database",
            "/data//base/",
            "/database/../",
            "/data%62ase/",
            "/_rws/",
            "/.well-known/",
        ] {
            let text = format!(
                "{base}[[sites.locations]]\npath='{prefix}'\nroot='{}'\n",
                application.display()
            );
            assert!(
                toml::from_str::<Config>(&text).unwrap().validate().is_err(),
                "{prefix}"
            );
        }
        for extra in [
            "deny_paths=['/vendor/../config']",
            "deny_files=['config**.php']",
            "php=true",
            "index='../index.php'",
            "front_controller='../index.php'",
        ] {
            let text = format!(
                "{base}[[sites.locations]]\npath='/database/'\nroot='{}'\n{extra}\n",
                application.display()
            );
            assert!(
                toml::from_str::<Config>(&text).unwrap().validate().is_err(),
                "{extra}"
            );
        }
        for prefix in ["/database/", "/database/nested/"] {
            let text = format!(
                "{base}{valid}[[sites.locations]]\npath='{prefix}'\nroot='{}'\n",
                another.display()
            );
            assert!(toml::from_str::<Config>(&text).unwrap().validate().is_err());
        }
    }

    #[test]
    fn authenticated_mount_requires_tls_and_password_files_outside_every_public_root() {
        let dir = tempdir().unwrap();
        let public = dir.path().join("public");
        let application = dir.path().join("application");
        fs::create_dir(&public).unwrap();
        fs::create_dir(&application).unwrap();
        let users = dir.path().join("users");
        fs::write(
            &users,
            format!("alice:{}\n", bcrypt::hash("password", 4).unwrap()),
        )
        .unwrap();
        let cert = dir.path().join("cert");
        fs::write(&cert, "placeholder").unwrap();
        let text = format!(
            "[tls]\ncert_file='{}'\nkey_file='{}'\n[[sites]]\nhost='example.test'\nroot='{}'\n[[sites.locations]]\npath='/database/'\nroot='{}'\n[sites.locations.basic_auth]\nrealm='Private'\nhtpasswd_file='{}'\n",
            cert.display(),
            cert.display(),
            public.display(),
            application.display(),
            users.display()
        );
        let config: Config = toml::from_str(&text).unwrap();
        config.validate().unwrap();
        let mut invalid = config.clone();
        invalid.tls = None;
        assert!(invalid.validate().is_err());
        let public_users = public.join("users");
        fs::copy(&users, &public_users).unwrap();
        let mut invalid = config;
        invalid.sites[0].locations[0]
            .basic_auth
            .as_mut()
            .unwrap()
            .htpasswd_file = public_users;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn validates_php_front_controller() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("index.php"), "<?php").unwrap();
        let text = format!(
            "[[sites]]\nhost='example.test'\nroot='{}'\nphp=true\nphp_fpm='unix:/run/php/php-fpm.sock'\nfront_controller='index.php'\n",
            dir.path().display()
        );
        let config: Config = toml::from_str(&text).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn validates_enabled_static_cache_limits() {
        let dir = tempdir().unwrap();
        let text = format!(
            "[static_cache]\nenabled=true\nmax_total_bytes=10\nmax_file_bytes=11\nmax_entries=1\n[[sites]]\nhost='example.test'\nroot='{}'\n",
            dir.path().display()
        );
        let config: Config = toml::from_str(&text).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_zero_timeouts_and_proxy_response_limit() {
        let dir = tempdir().unwrap();
        for setting in [
            "request_timeout_ms=0",
            "shutdown_grace_period_s=0",
            "max_proxy_response_bytes=0",
            "max_fastcgi_response_bytes=0",
        ] {
            let text = format!(
                "[server]\n{setting}\n[[sites]]\nhost='example.test'\nroot='{}'\n",
                dir.path().display()
            );
            let config: Config = toml::from_str(&text).unwrap();
            assert!(config.validate().is_err(), "accepted {setting}");
        }
    }

    #[tokio::test]
    async fn rejects_invalid_tls_certificate_contents() {
        let dir = tempdir().unwrap();
        let cert = dir.path().join("certificate.pem");
        let key = dir.path().join("key.pem");
        fs::write(&cert, "not a certificate").unwrap();
        fs::write(&key, "not a private key").unwrap();
        let text = format!(
            "[tls]\ncert_file='{}'\nkey_file='{}'\n[[sites]]\nhost='example.test'\nroot='{}'\n",
            cert.display(),
            key.display(),
            dir.path().display()
        );
        let config: Config = toml::from_str(&text).unwrap();
        config.validate().unwrap();
        assert!(config.validate_tls().await.is_err());
    }
}
