use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use rws_bench::{
    check, report,
    run::{self, RunConfig},
    ws::{self, WebSocketConfig},
};
use std::{net::IpAddr, path::PathBuf};

#[derive(Debug, Parser)]
#[command(
    name = "rws-bench",
    version,
    about = "Deterministic RWS/Nginx socket scalability benchmark"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect load-generator and target resource limits without changing the system.
    Check(CheckArgs),
    /// Run one guarded HTTP/1.1 keep-alive workload and write raw JSON.
    Run(Box<RunArgs>),
    /// Run a guarded mixed-idle WebSocket echo workload for scheduler comparisons.
    Websocket(Box<WebSocketArgs>),
    /// Generate a complete Markdown report from raw JSON runs.
    Report(ReportArgs),
    /// Compare variants or merge simultaneous load generators.
    Compare(ReportArgs),
}

#[derive(Debug, Args)]
struct WebSocketArgs {
    #[arg(long)]
    url: String,
    #[arg(long, default_value = "bench.local")]
    host: String,
    #[arg(long)]
    label: String,
    #[arg(long, default_value_t = 100)]
    connections: usize,
    #[arg(long, default_value_t = 1.0)]
    active_percent: f64,
    #[arg(long, default_value_t = 10)]
    idle_duration: u64,
    #[arg(long, default_value_t = 10)]
    active_duration: u64,
    #[arg(long, default_value_t = 100)]
    rps: u64,
    #[arg(long, default_value_t = 32)]
    payload_bytes: usize,
    #[arg(long, default_value_t = 1000)]
    connect_rate: u64,
    #[arg(long, default_value_t = 10000)]
    request_timeout_ms: u64,
    #[arg(long)]
    allow_large_test: bool,
    #[arg(long)]
    monitor_ssh: Option<String>,
    #[arg(long)]
    monitor_process: Option<String>,
    #[arg(long, default_value_t = 80)]
    monitor_port: u16,
    #[arg(long, default_value = "rws-websocket-results.json")]
    output: PathBuf,
}

#[derive(Debug, Args)]
struct CheckArgs {
    #[arg(long)]
    target: Option<String>,
    #[arg(long, default_value_t = 100_000)]
    connections: usize,
    #[arg(long = "source-ip")]
    source_ips: Vec<IpAddr>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Optional TOML configuration; explicit CLI values override it.
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    target: Option<String>,
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    label: Option<String>,
    #[arg(long)]
    connections: Option<usize>,
    #[arg(long)]
    active_percent: Option<f64>,
    #[arg(long)]
    idle_duration: Option<u64>,
    #[arg(long)]
    active_duration: Option<u64>,
    #[arg(long)]
    rps: Option<u64>,
    #[arg(long)]
    connect_rate: Option<u64>,
    #[arg(long)]
    request_timeout_ms: Option<u64>,
    #[arg(long = "source-ip")]
    source_ips: Vec<IpAddr>,
    /// Required above 10,000 connections. This does not bypass OS limits.
    #[arg(long)]
    allow_large_test: bool,
    /// SSH destination used only for periodic read-only monitoring.
    #[arg(long)]
    monitor_ssh: Option<String>,
    /// Exact process name to monitor, for example rsw-simple or nginx.
    #[arg(long)]
    monitor_process: Option<String>,
    #[arg(long)]
    monitor_port: Option<u16>,
    #[arg(long)]
    monitor_interval_ms: Option<u64>,
    #[arg(long, default_value = "rws-results.json")]
    output: PathBuf,
}

#[derive(Debug, Args)]
struct ReportArgs {
    /// Raw JSON files from one or more runs/load generators.
    #[arg(required = true)]
    inputs: Vec<PathBuf>,
    #[arg(short, long, default_value = "RWS_BENCHMARK_REPORT.md")]
    output: PathBuf,
    /// Sum runs sharing a label as simultaneous load generators instead of taking repetition medians.
    #[arg(long)]
    merge_load_generators: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Check(args) => {
            let value = check::inspect(
                args.target.as_deref(),
                args.connections,
                args.source_ips.len(),
            )
            .await?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                check::print_human(&value);
            }
        }
        Command::Run(args) => {
            let output = args.output.clone();
            let config = apply_run_args(*args)?;
            let result = run::execute(config).await?;
            run::write_json(&output, &result)?;
            println!("wrote {}", output.display());
            println!("connections_opened={}", result.summary.connections_opened);
            println!("responses_ok={}", result.summary.responses_ok);
            println!("achieved_rps={:.2}", result.summary.achieved_rps);
            println!("latency_p99_us={}", result.summary.request_latency.p99_us);
            for warning in result.warnings {
                eprintln!("WARNING: {warning}");
            }
        }
        Command::Websocket(args) => {
            let result = ws::execute(WebSocketConfig {
                label: args.label.clone(),
                url: args.url.clone(),
                host: args.host.clone(),
                connections: args.connections,
                active_percent: args.active_percent,
                idle_duration_secs: args.idle_duration,
                active_duration_secs: args.active_duration,
                rps: args.rps,
                payload_bytes: args.payload_bytes,
                connect_rate: args.connect_rate,
                request_timeout_ms: args.request_timeout_ms,
                allow_large_test: args.allow_large_test,
                monitor_ssh: args.monitor_ssh.clone(),
                monitor_process: args.monitor_process.clone(),
                monitor_port: args.monitor_port,
            })
            .await?;
            ws::write_json(&args.output, &result)?;
            println!("wrote {}", args.output.display());
            println!("connections_opened={}", result.summary.connections_opened);
            println!("messages_echoed={}", result.summary.responses_ok);
            println!("achieved_rps={:.2}", result.summary.achieved_rps);
            println!("latency_p99_us={}", result.summary.request_latency.p99_us);
        }
        Command::Report(args) | Command::Compare(args) => {
            report::write_report(&args.inputs, &args.output, args.merge_load_generators)?;
            println!("wrote {}", args.output.display());
        }
    }
    Ok(())
}

fn apply_run_args(args: RunArgs) -> Result<RunConfig> {
    let mut config = match &args.config {
        Some(path) => RunConfig::load(path)?,
        None => RunConfig::default(),
    };
    if let Some(value) = args.target {
        config.target = value;
    }
    if let Some(value) = args.host {
        config.host = value;
    }
    if let Some(value) = args.label {
        config.label = value;
    }
    if let Some(value) = args.connections {
        config.connections = value;
    }
    if let Some(value) = args.active_percent {
        config.active_percent = value;
    }
    if let Some(value) = args.idle_duration {
        config.idle_duration_secs = value;
    }
    if let Some(value) = args.active_duration {
        config.active_duration_secs = value;
    }
    if let Some(value) = args.rps {
        config.rps = value;
    }
    if let Some(value) = args.connect_rate {
        config.connect_rate = value;
    }
    if let Some(value) = args.request_timeout_ms {
        config.request_timeout_ms = value;
    }
    if !args.source_ips.is_empty() {
        config.source_ips = args.source_ips;
    }
    if args.allow_large_test {
        config.allow_large_test = true;
    }
    if let Some(value) = args.monitor_ssh {
        config.monitor_ssh = Some(value);
    }
    if let Some(value) = args.monitor_process {
        config.monitor_process = Some(value);
    }
    if let Some(value) = args.monitor_port {
        config.monitor_port = value;
    }
    if let Some(value) = args.monitor_interval_ms {
        config.monitor_interval_ms = value;
    }
    Ok(config)
}
