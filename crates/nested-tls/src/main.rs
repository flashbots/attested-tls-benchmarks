use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, ValueEnum};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_rustls::{
    rustls::{
        ClientConfig, RootCertStore, ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
    },
    TlsAcceptor, TlsConnector,
};

const DEFAULT_MODES: &str = "single-tls,nested-tls";
const DEFAULT_PAYLOAD_SIZES: &str = "65536,1048576,16777216";
const DEFAULT_CHUNK_BYTES: usize = 16384;
const DEFAULT_ROUNDS: u64 = 10;
const DEFAULT_CONCURRENCY: usize = 1;
const DEFAULT_SERVER_IP: &str = "127.0.0.1";
const DEFAULT_RESULTS_DIR: &str = "results";
const ROUND_TIMEOUT_SECS: u64 = 30;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, ValueEnum)]
enum Mode {
    SingleTls,
    NestedTls,
}

#[derive(Debug, Parser)]
#[command(name = "nested-tls-benchmark")]
#[command(about = "Benchmark single TLS vs nested TLS upload throughput")]
struct Cli {
    /// Comma-delimited benchmark modes.
    #[arg(long, value_enum, value_delimiter = ',', default_value = DEFAULT_MODES)]
    modes: Vec<Mode>,
    /// Comma-delimited payload sizes in bytes.
    #[arg(long, value_delimiter = ',', default_value = DEFAULT_PAYLOAD_SIZES)]
    payload_sizes: Vec<usize>,
    /// Bytes per write operation from client to server.
    #[arg(long, default_value_t = DEFAULT_CHUNK_BYTES)]
    chunk_bytes: usize,
    /// Number of rounds per mode/payload row.
    #[arg(long, default_value_t = DEFAULT_ROUNDS)]
    rounds: u64,
    /// Number of parallel connections per round.
    #[arg(long, default_value_t = DEFAULT_CONCURRENCY)]
    concurrency: usize,
    /// Directory for timestamped CSV output when --csv-path is not provided.
    #[arg(long, default_value = DEFAULT_RESULTS_DIR)]
    results_dir: PathBuf,
    /// Explicit CSV output file path. Overrides --results-dir timestamped path.
    #[arg(long)]
    csv_path: Option<PathBuf>,
    /// Server bind/listen IP for benchmark rounds.
    #[arg(long, default_value = DEFAULT_SERVER_IP)]
    server_ip: IpAddr,
    /// Optional interface to sample wire byte/packet counters from (for example `lo` or `eth0`).
    #[arg(long)]
    netdev: Option<String>,
}

#[derive(Debug, Clone)]
struct Stats {
    mean: f64,
    p50: f64,
    p95: f64,
}

#[derive(Debug, Clone)]
struct RowResult {
    mode: Mode,
    payload_bytes: usize,
    chunk_bytes: usize,
    concurrency: usize,
    rounds: u64,
    success_rounds: u64,
    failures: u64,
    total_bytes: u64,
    wall_time: Duration,
    stats: Option<Stats>,
    cpu_user_ms: Option<f64>,
    cpu_system_ms: Option<f64>,
    cpu_total_ms: Option<f64>,
    cpu_ms_per_mib: Option<f64>,
    wire_tx_bytes: Option<u64>,
    wire_rx_bytes: Option<u64>,
    wire_tx_packets: Option<u64>,
    wire_rx_packets: Option<u64>,
    wire_bytes_per_app_byte: Option<f64>,
    overhead_vs_single_pct: Option<f64>,
}

#[derive(Debug)]
enum BenchError {
    Io(io::Error),
    Timeout(String),
    Protocol(String),
}

impl std::fmt::Display for BenchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BenchError::Io(err) => write!(f, "{err}"),
            BenchError::Timeout(msg) => write!(f, "{msg}"),
            BenchError::Protocol(msg) => write!(f, "{msg}"),
        }
    }
}

impl From<io::Error> for BenchError {
    fn from(value: io::Error) -> Self {
        BenchError::Io(value)
    }
}

#[derive(Copy, Clone, Debug, Default)]
struct CpuTimes {
    user_ms: f64,
    system_ms: f64,
}

#[derive(Copy, Clone, Debug, Default)]
struct NetCounters {
    tx_bytes: u64,
    rx_bytes: u64,
    tx_packets: u64,
    rx_packets: u64,
}

/// Helper to generate a self-signed certificate for testing.
pub fn generate_certificate_chain(
    ip: IpAddr,
) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let mut params = rcgen::CertificateParams::new(vec![]).unwrap();
    params.subject_alt_names.push(rcgen::SanType::IpAddress(ip));
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, ip.to_string());

    let keypair = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&keypair).unwrap();

    let certs = vec![cert.der().clone()];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(keypair.serialize_der()));
    (certs, key)
}

/// Helper to generate TLS configuration for testing.
pub fn generate_tls_config(
    certificate_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> (ServerConfig, ClientConfig) {
    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificate_chain.clone(), key)
        .expect("Failed to create rustls server config");

    let mut root_store = RootCertStore::empty();
    root_store.add(certificate_chain[0].clone()).unwrap();

    let client_config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    (server_config, client_config)
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(err) = validate_args(&cli) {
        eprintln!("invalid arguments: {err}");
        std::process::exit(2);
    }

    if let Err(err) = fs::create_dir_all(&cli.results_dir) {
        eprintln!(
            "failed to create results dir {}: {err}",
            cli.results_dir.display()
        );
        std::process::exit(1);
    }

    let csv_path = cli
        .csv_path
        .clone()
        .unwrap_or_else(|| default_csv_path(&cli.results_dir));
    if let Some(parent) = csv_path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            eprintln!(
                "failed to create csv parent dir {}: {err}",
                parent.display()
            );
            std::process::exit(1);
        }
    }

    let tls = build_tls_materials(cli.server_ip);
    let mut rows = Vec::with_capacity(cli.modes.len() * cli.payload_sizes.len());

    for &mode in &cli.modes {
        for &payload_bytes in &cli.payload_sizes {
            match run_row(&cli, mode, payload_bytes, &tls).await {
                Ok(row) => rows.push(row),
                Err(err) => {
                    eprintln!(
                        "benchmark failed for mode {} payload={} bytes: {err}",
                        mode_name(mode),
                        payload_bytes
                    );
                    std::process::exit(1);
                }
            }
        }
    }

    compute_overheads(&mut rows);
    print_table(&rows);

    let row_timestamp = unix_timestamp_string();
    if let Err(err) = write_csv(&csv_path, &row_timestamp, &rows) {
        eprintln!("failed to write csv {}: {err}", csv_path.display());
        std::process::exit(1);
    }
    println!("csv_path={}", csv_path.display());
}

#[derive(Clone)]
struct TlsMaterials {
    outer_acceptor: TlsAcceptor,
    outer_connector: TlsConnector,
    inner_acceptor: TlsAcceptor,
    inner_connector: TlsConnector,
}

fn build_tls_materials(ip: IpAddr) -> TlsMaterials {
    let (outer_chain, outer_key) = generate_certificate_chain(ip);
    let (outer_server_config, outer_client_config) = generate_tls_config(outer_chain, outer_key);

    let (inner_chain, inner_key) = generate_certificate_chain(ip);
    let (inner_server_config, inner_client_config) = generate_tls_config(inner_chain, inner_key);

    TlsMaterials {
        outer_acceptor: TlsAcceptor::from(Arc::new(outer_server_config)),
        outer_connector: TlsConnector::from(Arc::new(outer_client_config)),
        inner_acceptor: TlsAcceptor::from(Arc::new(inner_server_config)),
        inner_connector: TlsConnector::from(Arc::new(inner_client_config)),
    }
}

fn validate_args(cli: &Cli) -> Result<(), String> {
    if cli.modes.is_empty() {
        return Err("at least one mode is required".to_owned());
    }
    if cli.payload_sizes.is_empty() {
        return Err("at least one payload size is required".to_owned());
    }
    if cli.payload_sizes.contains(&0) {
        return Err("payload sizes must be > 0".to_owned());
    }
    if cli.chunk_bytes == 0 {
        return Err("chunk-bytes must be > 0".to_owned());
    }
    if cli.rounds == 0 {
        return Err("rounds must be > 0".to_owned());
    }
    if cli.concurrency == 0 {
        return Err("concurrency must be > 0".to_owned());
    }
    Ok(())
}

fn default_csv_path(results_dir: &Path) -> PathBuf {
    let ts = unix_timestamp_string().replace('.', "_");
    results_dir.join(format!("{ts}-tls_upload.csv"))
}

fn unix_timestamp_string() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => format!("{}.{}", d.as_secs(), d.subsec_nanos()),
        Err(_) => "0.0".to_owned(),
    }
}

async fn run_row(
    cli: &Cli,
    mode: Mode,
    payload_bytes: usize,
    tls: &TlsMaterials,
) -> Result<RowResult, BenchError> {
    let cpu_before = cpu_times()?;
    let net_before = if let Some(netdev) = &cli.netdev {
        match read_netdev_counters(netdev) {
            Ok(counters) => Some(counters),
            Err(err) => {
                eprintln!(
                    "warning: failed to read netdev counters for {} (before row): {err}",
                    netdev
                );
                None
            }
        }
    } else {
        None
    };

    let mut failures = 0_u64;
    let mut successes = 0_u64;
    let mut last_round_error: Option<String> = None;
    let mut per_round_throughput = Vec::with_capacity(cli.rounds as usize);
    let mut total_bytes = 0_u64;
    let row_start = Instant::now();

    for _ in 0..cli.rounds {
        match timeout(
            Duration::from_secs(ROUND_TIMEOUT_SECS),
            run_one_round(
                mode,
                cli.server_ip,
                payload_bytes,
                cli.chunk_bytes,
                cli.concurrency,
                tls,
            ),
        )
        .await
        {
            Ok(Ok(throughput_bytes_per_sec)) => {
                per_round_throughput.push(throughput_bytes_per_sec);
                successes += 1;
                total_bytes += payload_bytes as u64 * cli.concurrency as u64;
            }
            Ok(Err(err)) => {
                failures += 1;
                last_round_error = Some(err.to_string());
            }
            Err(_) => {
                return Err(BenchError::Timeout(format!(
                    "round exceeded {ROUND_TIMEOUT_SECS}s timeout"
                )))
            }
        }
    }

    if successes == 0 && failures > 0 {
        return Err(BenchError::Protocol(format!(
            "all rounds failed (mode={}, payload_bytes={}): {}",
            mode_name(mode),
            payload_bytes,
            last_round_error.unwrap_or_else(|| "unknown round error".to_owned())
        )));
    }

    let wall_time = row_start.elapsed();
    let stats = calculate_stats(&per_round_throughput);
    let cpu_after = cpu_times()?;
    let cpu_user_ms = Some((cpu_after.user_ms - cpu_before.user_ms).max(0.0));
    let cpu_system_ms = Some((cpu_after.system_ms - cpu_before.system_ms).max(0.0));
    let cpu_total_ms = Some(cpu_user_ms.unwrap_or(0.0) + cpu_system_ms.unwrap_or(0.0));
    let cpu_ms_per_mib = if total_bytes > 0 {
        Some(cpu_total_ms.unwrap_or(0.0) / (total_bytes as f64 / (1024.0 * 1024.0)))
    } else {
        None
    };

    let (wire_tx_bytes, wire_rx_bytes, wire_tx_packets, wire_rx_packets, wire_bytes_per_app_byte) =
        if let (Some(netdev), Some(before)) = (&cli.netdev, net_before) {
            match read_netdev_counters(netdev) {
                Ok(after) => {
                    let tx = after.tx_bytes.saturating_sub(before.tx_bytes);
                    let rx = after.rx_bytes.saturating_sub(before.rx_bytes);
                    let tx_pkts = after.tx_packets.saturating_sub(before.tx_packets);
                    let rx_pkts = after.rx_packets.saturating_sub(before.rx_packets);
                    let ratio = if total_bytes > 0 {
                        Some((tx + rx) as f64 / total_bytes as f64)
                    } else {
                        None
                    };
                    (Some(tx), Some(rx), Some(tx_pkts), Some(rx_pkts), ratio)
                }
                Err(err) => {
                    eprintln!(
                        "warning: failed to read netdev counters for {} (after row): {err}",
                        netdev
                    );
                    (None, None, None, None, None)
                }
            }
        } else {
            (None, None, None, None, None)
        };

    Ok(RowResult {
        mode,
        payload_bytes,
        chunk_bytes: cli.chunk_bytes,
        concurrency: cli.concurrency,
        rounds: cli.rounds,
        success_rounds: successes,
        failures,
        total_bytes,
        wall_time,
        stats,
        cpu_user_ms,
        cpu_system_ms,
        cpu_total_ms,
        cpu_ms_per_mib,
        wire_tx_bytes,
        wire_rx_bytes,
        wire_tx_packets,
        wire_rx_packets,
        wire_bytes_per_app_byte,
        overhead_vs_single_pct: None,
    })
}

async fn run_one_round(
    mode: Mode,
    server_ip: IpAddr,
    payload_bytes: usize,
    chunk_bytes: usize,
    concurrency: usize,
    tls: &TlsMaterials,
) -> Result<f64, BenchError> {
    let round_start = Instant::now();
    let mut handles = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let tls = tls.clone();
        handles.push(tokio::spawn(run_one_connection(
            mode,
            server_ip,
            payload_bytes,
            chunk_bytes,
            tls,
        )));
    }

    for handle in handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(err) => {
                return Err(BenchError::Protocol(format!(
                    "connection task join error: {err}"
                )));
            }
        }
    }

    let elapsed = round_start.elapsed();
    let total_payload_bytes = payload_bytes as f64 * concurrency as f64;
    let throughput = total_payload_bytes / elapsed.as_secs_f64();
    Ok(throughput)
}

async fn run_one_connection(
    mode: Mode,
    server_ip: IpAddr,
    payload_bytes: usize,
    chunk_bytes: usize,
    tls: TlsMaterials,
) -> Result<(), BenchError> {
    let listener = TcpListener::bind((server_ip, 0)).await?;
    let server_addr = listener.local_addr()?;
    let server_name = ServerName::try_from(server_addr.ip())
        .map_err(|err| BenchError::Protocol(format!("invalid server name: {err}")))?;

    let outer_acceptor = tls.outer_acceptor.clone();
    let inner_acceptor = tls.inner_acceptor.clone();
    let server_mode = mode;
    let server_task = tokio::spawn(async move {
        let (tcp_stream, _) = listener.accept().await?;
        match server_mode {
            Mode::SingleTls => {
                let mut stream = outer_acceptor.accept(tcp_stream).await?;
                drain_exact_bytes(&mut stream, payload_bytes).await?;
                send_ack(&mut stream).await
            }
            Mode::NestedTls => {
                let outer_stream = outer_acceptor.accept(tcp_stream).await?;
                let mut inner_stream = inner_acceptor.accept(outer_stream).await?;
                drain_exact_bytes(&mut inner_stream, payload_bytes).await?;
                send_ack(&mut inner_stream).await
            }
        }
    });

    let outer_connector = tls.outer_connector.clone();
    let inner_connector = tls.inner_connector.clone();
    let tcp_stream = TcpStream::connect(server_addr).await?;
    match mode {
        Mode::SingleTls => {
            let mut stream = outer_connector.connect(server_name.clone(), tcp_stream).await?;
            write_exact_bytes(&mut stream, payload_bytes, chunk_bytes).await?;
            expect_ack(&mut stream).await?;
        }
        Mode::NestedTls => {
            let outer_stream = outer_connector.connect(server_name.clone(), tcp_stream).await?;
            let mut inner_stream = inner_connector.connect(server_name, outer_stream).await?;
            write_exact_bytes(&mut inner_stream, payload_bytes, chunk_bytes).await?;
            expect_ack(&mut inner_stream).await?;
        }
    }

    match server_task.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(BenchError::Io(err)),
        Err(err) => Err(BenchError::Protocol(format!(
            "server task join error: {err}"
        ))),
    }
}

async fn write_exact_bytes<S>(
    stream: &mut S,
    payload_bytes: usize,
    chunk_bytes: usize,
) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let chunk = vec![0xAB_u8; chunk_bytes];
    let mut remaining = payload_bytes;

    while remaining > 0 {
        let to_write = remaining.min(chunk.len());
        stream.write_all(&chunk[..to_write]).await?;
        remaining -= to_write;
    }
    stream.flush().await?;
    Ok(())
}

async fn drain_exact_bytes<S>(stream: &mut S, payload_bytes: usize) -> io::Result<()>
where
    S: AsyncRead + Unpin,
{
    let mut remaining = payload_bytes;
    let mut buffer = vec![0_u8; 16 * 1024];

    while remaining > 0 {
        let max_read = remaining.min(buffer.len());
        let n = stream.read(&mut buffer[..max_read]).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("stream closed with {remaining} bytes remaining"),
            ));
        }
        remaining -= n;
    }
    Ok(())
}

async fn send_ack<S>(stream: &mut S) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    stream.write_all(&[0xA5]).await?;
    stream.flush().await
}

async fn expect_ack<S>(stream: &mut S) -> io::Result<()>
where
    S: AsyncRead + Unpin,
{
    let mut ack = [0_u8; 1];
    stream.read_exact(&mut ack).await?;
    if ack[0] != 0xA5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected ack byte: {}", ack[0]),
        ));
    }
    Ok(())
}

fn calculate_stats(samples: &[f64]) -> Option<Stats> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
    Some(Stats {
        mean,
        p50: percentile_nearest_rank(&sorted, 50),
        p95: percentile_nearest_rank(&sorted, 95),
    })
}

fn percentile_nearest_rank(sorted_samples: &[f64], percentile: usize) -> f64 {
    if sorted_samples.is_empty() {
        return f64::NAN;
    }
    let p = percentile.clamp(1, 100) as f64 / 100.0;
    let rank = (p * sorted_samples.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted_samples.len() - 1);
    sorted_samples[idx]
}

fn compute_overheads(rows: &mut [RowResult]) {
    let mut single_by_payload: HashMap<(usize, usize), f64> = HashMap::new();
    for row in rows.iter() {
        if row.mode == Mode::SingleTls {
            if let Some(stats) = &row.stats {
                single_by_payload.insert((row.payload_bytes, row.concurrency), stats.mean);
            }
        }
    }

    for row in rows.iter_mut() {
        if row.mode != Mode::NestedTls {
            continue;
        }
        let Some(nested_stats) = &row.stats else {
            row.overhead_vs_single_pct = None;
            continue;
        };
        let Some(single_mean) = single_by_payload.get(&(row.payload_bytes, row.concurrency)) else {
            row.overhead_vs_single_pct = None;
            continue;
        };
        if *single_mean <= 0.0 {
            row.overhead_vs_single_pct = None;
            continue;
        }
        row.overhead_vs_single_pct = Some(((*single_mean - nested_stats.mean) / *single_mean) * 100.0);
    }
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::SingleTls => "single_tls",
        Mode::NestedTls => "nested_tls",
    }
}

fn print_table(rows: &[RowResult]) {
    println!(
        "{:<12} {:<12} {:<12} {:<12} {:<8} {:<8} {:<12} {:<10} {:<10} {:<12}",
        "mode",
        "payload_bytes",
        "chunk_bytes",
        "concurrency",
        "rounds",
        "fail",
        "mean_MiB/s",
        "cpu_ms/MiB",
        "wire/app",
        "overhead_%"
    );

    for row in rows {
        let mean_mib_s = row
            .stats
            .as_ref()
            .map(|stats| format!("{:.3}", stats.mean / (1024.0 * 1024.0)))
            .unwrap_or_else(|| "NaN".to_owned());
        let cpu_per_mib = row
            .cpu_ms_per_mib
            .map(|v| format!("{:.3}", v))
            .unwrap_or_else(|| "NaN".to_owned());
        let wire_per_app = row
            .wire_bytes_per_app_byte
            .map(|v| format!("{:.3}", v))
            .unwrap_or_else(|| "NaN".to_owned());
        let overhead = row
            .overhead_vs_single_pct
            .map(|v| format!("{:.3}", v))
            .unwrap_or_else(|| "NaN".to_owned());
        println!(
            "{:<12} {:<12} {:<12} {:<12} {:<8} {:<8} {:<12} {:<10} {:<10} {:<12}",
            mode_name(row.mode),
            row.payload_bytes,
            row.chunk_bytes,
            row.concurrency,
            row.rounds,
            row.failures,
            mean_mib_s,
            cpu_per_mib,
            wire_per_app,
            overhead
        );
    }
}

fn write_csv(csv_path: &Path, timestamp: &str, rows: &[RowResult]) -> io::Result<()> {
    let file = File::create(csv_path)?;
    let mut writer = BufWriter::new(file);
    writeln!(
        writer,
        "timestamp,benchmark,mode,payload_bytes,chunk_bytes,concurrency,rounds,success_rounds,failures,total_bytes,wall_time_ms,throughput_bytes_s_mean,throughput_mib_s_mean,throughput_bytes_s_p50,throughput_bytes_s_p95,cpu_user_ms,cpu_system_ms,cpu_total_ms,cpu_ms_per_mib,wire_tx_bytes,wire_rx_bytes,wire_tx_packets,wire_rx_packets,wire_bytes_per_app_byte,overhead_vs_single_pct"
    )?;

    for row in rows {
        let (mean, p50, p95) = match &row.stats {
            Some(stats) => (stats.mean, stats.p50, stats.p95),
            None => (f64::NAN, f64::NAN, f64::NAN),
        };
        let overhead = row.overhead_vs_single_pct.unwrap_or(f64::NAN);
        let mean_mib_s = mean / (1024.0 * 1024.0);
        let cpu_user_ms = row.cpu_user_ms.unwrap_or(f64::NAN);
        let cpu_system_ms = row.cpu_system_ms.unwrap_or(f64::NAN);
        let cpu_total_ms = row.cpu_total_ms.unwrap_or(f64::NAN);
        let cpu_ms_per_mib = row.cpu_ms_per_mib.unwrap_or(f64::NAN);
        let wire_tx_bytes = row.wire_tx_bytes.unwrap_or(0);
        let wire_rx_bytes = row.wire_rx_bytes.unwrap_or(0);
        let wire_tx_packets = row.wire_tx_packets.unwrap_or(0);
        let wire_rx_packets = row.wire_rx_packets.unwrap_or(0);
        let wire_bytes_per_app_byte = row.wire_bytes_per_app_byte.unwrap_or(f64::NAN);
        writeln!(
            writer,
            "{},tls_upload,{},{},{},{},{},{},{},{},{:.3},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{},{},{},{},{:.6},{:.6}",
            timestamp,
            mode_name(row.mode),
            row.payload_bytes,
            row.chunk_bytes,
            row.concurrency,
            row.rounds,
            row.success_rounds,
            row.failures,
            row.total_bytes,
            row.wall_time.as_secs_f64() * 1_000.0,
            mean,
            mean_mib_s,
            p50,
            p95,
            cpu_user_ms,
            cpu_system_ms,
            cpu_total_ms,
            cpu_ms_per_mib,
            wire_tx_bytes,
            wire_rx_bytes,
            wire_tx_packets,
            wire_rx_packets,
            wire_bytes_per_app_byte,
            overhead
        )?;
    }
    writer.flush()
}

fn cpu_times() -> io::Result<CpuTimes> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: We pass a valid pointer and immediately read the initialized value only on success.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: getrusage succeeded and initialized `usage`.
    let usage = unsafe { usage.assume_init() };
    let user_ms =
        usage.ru_utime.tv_sec as f64 * 1_000.0 + usage.ru_utime.tv_usec as f64 / 1_000.0;
    let system_ms =
        usage.ru_stime.tv_sec as f64 * 1_000.0 + usage.ru_stime.tv_usec as f64 / 1_000.0;
    Ok(CpuTimes { user_ms, system_ms })
}

fn read_counter_value(path: &Path) -> io::Result<u64> {
    let contents = fs::read_to_string(path)?;
    contents
        .trim()
        .parse::<u64>()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn read_netdev_counters(netdev: &str) -> io::Result<NetCounters> {
    let base = Path::new("/sys/class/net")
        .join(netdev)
        .join("statistics");
    Ok(NetCounters {
        tx_bytes: read_counter_value(&base.join("tx_bytes"))?,
        rx_bytes: read_counter_value(&base.join("rx_bytes"))?,
        tx_packets: read_counter_value(&base.join("tx_packets"))?,
        rx_packets: read_counter_value(&base.join("rx_packets"))?,
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Mode, RowResult, Stats, calculate_stats, compute_overheads, percentile_nearest_rank};
    use std::time::Duration;

    #[test]
    fn percentile_uses_nearest_rank() {
        let samples = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile_nearest_rank(&samples, 50), 3.0);
        assert_eq!(percentile_nearest_rank(&samples, 95), 5.0);
    }

    #[test]
    fn calculate_stats_returns_expected_values() {
        let samples = vec![2.0, 4.0, 6.0, 8.0];
        let stats = calculate_stats(&samples).expect("stats should be present");
        assert!((stats.mean - 5.0).abs() < 1e-9);
        assert_eq!(stats.p50, 4.0);
        assert_eq!(stats.p95, 8.0);
    }

    #[test]
    fn cli_parses_modes_and_payload_sizes() {
        let cli = Cli::try_parse_from([
            "bin",
            "--modes",
            "single-tls,nested-tls",
            "--payload-sizes",
            "4096,8192",
            "--concurrency",
            "4",
        ])
        .expect("cli should parse");
        assert_eq!(cli.modes, vec![Mode::SingleTls, Mode::NestedTls]);
        assert_eq!(cli.payload_sizes, vec![4096, 8192]);
        assert_eq!(cli.concurrency, 4);
    }

    #[test]
    fn compute_overheads_sets_nested_values_when_single_available() {
        let mut rows = vec![
            RowResult {
                mode: Mode::SingleTls,
                payload_bytes: 1024,
                chunk_bytes: 512,
                concurrency: 2,
                rounds: 2,
                success_rounds: 2,
                failures: 0,
                total_bytes: 2048,
                wall_time: Duration::from_millis(10),
                stats: Some(Stats {
                    mean: 1000.0,
                    p50: 1000.0,
                    p95: 1000.0,
                }),
                cpu_user_ms: None,
                cpu_system_ms: None,
                cpu_total_ms: None,
                cpu_ms_per_mib: None,
                wire_tx_bytes: None,
                wire_rx_bytes: None,
                wire_tx_packets: None,
                wire_rx_packets: None,
                wire_bytes_per_app_byte: None,
                overhead_vs_single_pct: None,
            },
            RowResult {
                mode: Mode::NestedTls,
                payload_bytes: 1024,
                chunk_bytes: 512,
                concurrency: 2,
                rounds: 2,
                success_rounds: 2,
                failures: 0,
                total_bytes: 2048,
                wall_time: Duration::from_millis(12),
                stats: Some(Stats {
                    mean: 800.0,
                    p50: 800.0,
                    p95: 800.0,
                }),
                cpu_user_ms: None,
                cpu_system_ms: None,
                cpu_total_ms: None,
                cpu_ms_per_mib: None,
                wire_tx_bytes: None,
                wire_rx_bytes: None,
                wire_tx_packets: None,
                wire_rx_packets: None,
                wire_bytes_per_app_byte: None,
                overhead_vs_single_pct: None,
            },
        ];
        compute_overheads(&mut rows);
        let overhead = rows[1].overhead_vs_single_pct.expect("overhead should exist");
        assert!((overhead - 20.0).abs() < 1e-9);
    }
}
