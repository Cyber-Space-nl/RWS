mod config;
#[cfg(feature = "comfy")]
pub(crate) mod controller;
mod metrics;
mod scheduler;
mod server;
mod site_log;
mod static_cache;

use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

pub use config::Config;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    Simple,
    Comfy,
}

impl Flavor {
    fn binary(self) -> &'static str {
        match self {
            Self::Simple => "rws-simple",
            Self::Comfy => "rws-comfy",
        }
    }

    fn default_config(self) -> &'static str {
        match self {
            Self::Simple => "/etc/rws-simple/config.toml",
            Self::Comfy => "/etc/rws-comfy/config.toml",
        }
    }
}

pub async fn run(flavor: Flavor) -> Result<()> {
    #[cfg(not(feature = "comfy"))]
    if flavor == Flavor::Comfy {
        bail!("rws-comfy requires the comfy build feature");
    }

    let mut config_path = PathBuf::from(flavor.default_config());
    let mut check = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help(flavor);
                return Ok(());
            }
            "-V" | "--version" => {
                println!("{} {}", flavor.binary(), env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--check" => check = true,
            "-c" | "--config" => {
                config_path = PathBuf::from(args.next().context("--config requires a path")?);
            }
            unknown => bail!("unknown argument '{unknown}'; use --help"),
        }
    }

    let config = Config::load(&config_path)?;
    if check {
        println!("{}: configuration is valid", config_path.display());
        return Ok(());
    }
    init_tracing(config.server.json_logs)?;
    tracing::info!(
        binary = flavor.binary(),
        config = %config_path.display(),
        "Rust WebServer starting"
    );
    server::serve(config).await
}

fn init_tracing(json: bool) -> Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("rws_core=info,info"));
    if json {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .try_init()
            .map_err(|error| anyhow::anyhow!("failed to initialize logging: {error}"))?;
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .try_init()
            .map_err(|error| anyhow::anyhow!("failed to initialize logging: {error}"))?;
    }
    Ok(())
}

fn print_help(flavor: Flavor) {
    println!(
        "{binary} {version}\nRust WebServer ({kind})\n\nUSAGE:\n    {binary} [OPTIONS]\n\nOPTIONS:\n    -c, --config PATH   Configuration file [default: {config}]\n        --check         Validate configuration and exit\n    -h, --help          Print help\n    -V, --version       Print version",
        binary = flavor.binary(),
        version = env!("CARGO_PKG_VERSION"),
        kind = match flavor {
            Flavor::Simple => "simple",
            Flavor::Comfy => "comfy",
        },
        config = flavor.default_config(),
    );
}
