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
    #[cfg(feature = "comfy")]
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

#[cfg(feature = "comfy")]
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdaptiveConfig {
    pub enabled: bool,
    pub interval_ms: u64,
    pub min_quantum_bytes: usize,
    pub max_quantum_bytes: usize,
    pub queue_high_watermark: usize,
}

#[cfg(feature = "comfy")]
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
    #[cfg(feature = "comfy")]
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
    pub php_fpm: Option<String>,
    pub front_controller: Option<String>,
    pub access_log: Option<PathBuf>,
    pub error_log: Option<PathBuf>,
    #[cfg(feature = "comfy")]
    pub upstream: Option<String>,
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
                #[cfg(feature = "comfy")]
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
        #[cfg(feature = "comfy")]
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
