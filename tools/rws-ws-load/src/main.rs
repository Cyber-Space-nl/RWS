use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use std::time::{Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Debug)]
struct Options {
    url: String,
    connections: usize,
    messages: usize,
    payload_bytes: usize,
    idle_seconds: u64,
    allow_large_test: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let options = parse_args()?;
    if options.connections > 10_000 && !options.allow_large_test {
        bail!("tests above 10,000 connections require --allow-large-test");
    }
    let started = Instant::now();
    let mut tasks = Vec::with_capacity(options.connections);
    for id in 0..options.connections {
        let url = options.url.clone();
        let messages = options.messages;
        let payload_bytes = options.payload_bytes;
        let idle_seconds = options.idle_seconds;
        tasks.push(tokio::spawn(async move {
            run_client(id, &url, messages, payload_bytes, idle_seconds).await
        }));
    }
    let mut latencies = Vec::new();
    let mut errors = 0usize;
    for task in tasks {
        match task.await {
            Ok(Ok(mut values)) => latencies.append(&mut values),
            Ok(Err(error)) => {
                errors += 1;
                eprintln!("client error: {error:#}");
            }
            Err(error) => {
                errors += 1;
                eprintln!("task error: {error}");
            }
        }
    }
    latencies.sort_unstable();
    println!("connections_requested={}", options.connections);
    println!("connection_errors={errors}");
    println!("messages_echoed={}", latencies.len());
    println!("elapsed_ms={}", started.elapsed().as_millis());
    if !latencies.is_empty() {
        println!("latency_p50_us={}", percentile(&latencies, 50).as_micros());
        println!("latency_p95_us={}", percentile(&latencies, 95).as_micros());
        println!("latency_p99_us={}", percentile(&latencies, 99).as_micros());
    }
    if errors > 0 {
        bail!("{errors} client(s) failed");
    }
    Ok(())
}

async fn run_client(
    id: usize,
    url: &str,
    messages: usize,
    payload_bytes: usize,
    idle_seconds: u64,
) -> Result<Vec<Duration>> {
    let (mut socket, _) = connect_async(url)
        .await
        .with_context(|| format!("connection {id} failed"))?;
    let payload = "x".repeat(payload_bytes);
    let mut latencies = Vec::with_capacity(messages);
    for _ in 0..messages {
        let started = Instant::now();
        socket.send(Message::Text(payload.clone().into())).await?;
        let response = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .context("echo timeout")?
            .context("socket closed before echo")??;
        if !matches!(response, Message::Text(_) | Message::Binary(_)) {
            bail!("unexpected echo frame");
        }
        latencies.push(started.elapsed());
    }
    if idle_seconds > 0 {
        tokio::time::sleep(Duration::from_secs(idle_seconds)).await;
    }
    socket.close(None).await?;
    Ok(latencies)
}

fn percentile(values: &[Duration], percentile: usize) -> Duration {
    let index = ((values.len() - 1) * percentile / 100).min(values.len() - 1);
    values[index]
}

fn parse_args() -> Result<Options> {
    let mut options = Options {
        url: "ws://127.0.0.1:8080/_rws/ws".to_owned(),
        connections: 100,
        messages: 1,
        payload_bytes: 32,
        idle_seconds: 0,
        allow_large_test: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--url" => options.url = args.next().context("--url requires a value")?,
            "--connections" => options.connections = parse_value(&mut args, "--connections")?,
            "--messages" => options.messages = parse_value(&mut args, "--messages")?,
            "--payload-bytes" => options.payload_bytes = parse_value(&mut args, "--payload-bytes")?,
            "--idle-seconds" => options.idle_seconds = parse_value(&mut args, "--idle-seconds")?,
            "--allow-large-test" => options.allow_large_test = true,
            "-h" | "--help" => {
                println!(
                    "rws-ws-load [--url URL] [--connections N] [--messages N] [--payload-bytes N] [--idle-seconds N] [--allow-large-test]"
                );
                std::process::exit(0);
            }
            unknown => bail!("unknown argument '{unknown}'"),
        }
    }
    if options.connections == 0 || options.payload_bytes == 0 {
        bail!("connections and payload-bytes must be greater than zero");
    }
    Ok(options)
}

fn parse_value<T: std::str::FromStr>(
    args: &mut impl Iterator<Item = String>,
    name: &str,
) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    args.next()
        .with_context(|| format!("{name} requires a value"))?
        .parse()
        .map_err(|error| anyhow::anyhow!("invalid value for {name}: {error}"))
}
