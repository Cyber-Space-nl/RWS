mod config;
pub(crate) mod controller;
mod metrics;
mod scheduler;
mod server;
mod site_log;
mod static_cache;
mod tls;

use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

pub use config::Config;

pub async fn run() -> Result<()> {
    let mut config_path = PathBuf::from("/etc/rws/config.toml");
    let mut check = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "-V" | "--version" => {
                println!("rws {}", env!("CARGO_PKG_VERSION"));
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
        config.validate_tls().await?;
        println!("{}: configuration is valid", config_path.display());
        return Ok(());
    }
    init_tracing(config.server.json_logs)?;
    tracing::info!(
        binary = "rws",
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

fn print_help() {
    println!(
        "rws {version}\nRust WebServer\n\nUSAGE:\n    rws [OPTIONS]\n\nOPTIONS:\n    -c, --config PATH   Configuration file [default: /etc/rws/config.toml]\n        --check         Validate configuration and exit\n    -h, --help          Print help\n    -V, --version       Print version",
        version = env!("CARGO_PKG_VERSION"),
    );
}
