use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use attested_tls::attestation::dcap::{
    DcapVerificationError, verify_dcap_attestation_with_given_timestamp,
};
use clap::{Parser, ValueEnum};
use configfs_tsm::QuoteGenerationError;
use dcap_qvl::QuoteCollateralV3;
use tokio::runtime::Handle;

const DEFAULT_CONCURRENCY: &str = "1,2,4,8,16,32,64,128";
const DEFAULT_VERIFY_QUOTE_PATH: &str =
    "crates/dcap-generate/test-data/dcap-tdx-1766059550570652607";
const DEFAULT_VERIFY_COLLATERAL_PATH: &str =
    "crates/dcap-generate/test-data/dcap-quote-collateral-00.json";
const DEFAULT_EXPECTED_INPUT_HEX: &str = "74276a648f1fd491f474a2d52c72d850e3768157b43ec297a9917482bd77278ba1882588391d1956b6f6466ad8b8dccd55f57221ad81b420f746fa8db0f8637d";
const DEFAULT_VERIFY_NOW: u64 = 1_769_509_141;

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum Workload {
    /// Benchmark `configfs-tsm::create_tdx_quote([0; 64])`.
    Tdx,
    /// Benchmark filesystem create/write/fsync/read/delete operations.
    Fs,
    /// Benchmark `attested_tls` DCAP verification using local quote/collateral test data.
    Verify,
}

#[derive(Debug, Parser)]
#[command(name = "dcap-generate-benchmark")]
#[command(about = "Benchmark DCAP-like generation via a filesystem stand-in workload")]
struct Cli {
    /// Comma-delimited worker counts to benchmark (for example: `1,2,4,8`).
    #[arg(long, value_delimiter = ',', default_value = DEFAULT_CONCURRENCY)]
    concurrency: Vec<usize>,
    /// Number of benchmark operations each worker performs at a given concurrency level.
    #[arg(long, default_value_t = 200)]
    iters_per_worker: u64,
    /// Bytes written/read per benchmark operation.
    #[arg(long, default_value_t = 4096)]
    payload_bytes: usize,
    /// Directory for timestamped CSV output when `--csv-path` is not provided.
    #[arg(long, default_value = "results")]
    results_dir: PathBuf,
    /// Explicit CSV output file path. Overrides the default timestamped path in `--results-dir`.
    #[arg(long)]
    csv_path: Option<PathBuf>,
    /// Directory used for temporary benchmark files. Defaults to the system temp directory.
    #[arg(long)]
    tmp_dir: Option<PathBuf>,
    /// Workload to benchmark: TDX quote generation, filesystem baseline, or verification.
    #[arg(long, value_enum, default_value_t = Workload::Tdx)]
    workload: Workload,
    /// Path to quote bytes used for verification workload.
    #[arg(long, default_value = DEFAULT_VERIFY_QUOTE_PATH)]
    verify_quote_path: PathBuf,
    /// Path to quote collateral JSON used for verification workload.
    #[arg(long, default_value = DEFAULT_VERIFY_COLLATERAL_PATH)]
    verify_collateral_path: PathBuf,
    /// Expected 64-byte quote report data as a hex string (128 hex chars).
    #[arg(long, default_value = DEFAULT_EXPECTED_INPUT_HEX)]
    expected_input_hex: String,
    /// UNIX timestamp used by verification workload.
    #[arg(long, default_value_t = DEFAULT_VERIFY_NOW)]
    verify_now: u64,
}

#[derive(Debug, Clone)]
struct LevelResult {
    concurrency: usize,
    success_ops: u64,
    failures: u64,
    wall_time: Duration,
    throughput_ops_per_sec: f64,
    stats: Option<LatencyStats>,
}

#[derive(Debug, Clone)]
struct LatencyStats {
    mean_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
}

#[derive(Debug, Clone)]
struct VerifyInputs {
    quote_bytes: Vec<u8>,
    collateral: QuoteCollateralV3,
    expected_input_data: [u8; 64],
    now: u64,
}

#[derive(Debug)]
enum WorkloadError {
    Fs(io::Error),
    Tdx(QuoteGenerationError),
    Verify(DcapVerificationError),
    VerifyConfig(String),
    TaskJoin(String),
}

impl std::fmt::Display for WorkloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkloadError::Fs(err) => write!(f, "filesystem operation failed: {err}"),
            WorkloadError::Tdx(err) => write!(f, "tdx quote generation failed: {err}"),
            WorkloadError::Verify(err) => write!(f, "dcap verification failed: {err}"),
            WorkloadError::VerifyConfig(err) => {
                write!(f, "invalid verification configuration: {err}")
            }
            WorkloadError::TaskJoin(err) => write!(f, "worker task failed: {err}"),
        }
    }
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
        .unwrap_or_else(|| default_csv_path(&cli.results_dir, cli.workload));
    if let Some(parent) = csv_path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            eprintln!(
                "failed to create csv parent dir {}: {err}",
                parent.display()
            );
            std::process::exit(1);
        }
    }

    let payload = vec![0xAB_u8; cli.payload_bytes];
    let tmp_base = cli.tmp_dir.clone().unwrap_or_else(std::env::temp_dir);
    let verify_inputs = match cli.workload {
        Workload::Verify => match load_verify_inputs(&cli) {
            Ok(inputs) => Some(Arc::new(inputs)),
            Err(err) => {
                eprintln!("failed to initialize verification inputs: {err}");
                std::process::exit(1);
            }
        },
        _ => None,
    };

    let mut all_results = Vec::with_capacity(cli.concurrency.len());
    for &concurrency in &cli.concurrency {
        match run_level(
            concurrency,
            cli.iters_per_worker,
            &payload,
            &tmp_base,
            cli.workload,
            verify_inputs.clone(),
        )
        .await
        {
            Ok(level) => all_results.push(level),
            Err(err) => {
                eprintln!(
                    "benchmark failed at concurrency {concurrency} for workload {}: {err}",
                    workload_name(cli.workload)
                );
                std::process::exit(1);
            }
        }
    }

    println!("workload={}", workload_name(cli.workload));
    print_table(&all_results);
    let row_timestamp = unix_timestamp_string();
    if let Err(err) = write_csv(
        &csv_path,
        &row_timestamp,
        cli.workload,
        cli.payload_bytes,
        cli.iters_per_worker,
        &all_results,
    ) {
        eprintln!("failed to write csv {}: {err}", csv_path.display());
        std::process::exit(1);
    }

    println!("csv_path={}", csv_path.display());

    let total_success: u64 = all_results.iter().map(|r| r.success_ops).sum();
    if total_success == 0 {
        eprintln!("no successful operations were recorded");
        std::process::exit(1);
    }
}

/// Validates CLI inputs and returns a user-facing error for invalid values.
fn validate_args(cli: &Cli) -> Result<(), String> {
    if cli.concurrency.is_empty() {
        return Err("at least one concurrency value is required".to_owned());
    }
    if cli.concurrency.contains(&0) {
        return Err("concurrency values must be > 0".to_owned());
    }
    if cli.iters_per_worker == 0 {
        return Err("iters-per-worker must be > 0".to_owned());
    }
    if cli.payload_bytes == 0 {
        return Err("payload-bytes must be > 0".to_owned());
    }
    if cli.workload == Workload::Verify {
        parse_expected_input_hex(&cli.expected_input_hex)
            .map_err(|err| format!("--expected-input-hex {err}"))?;
    }
    Ok(())
}

/// Builds the default timestamped CSV output path under the results directory.
fn default_csv_path(results_dir: &Path, workload: Workload) -> PathBuf {
    let ts = unix_timestamp_string().replace('.', "_");
    results_dir.join(format!("{ts}-{}.csv", benchmark_name(workload)))
}

/// Returns the current UNIX timestamp as a string with subsecond precision.
fn unix_timestamp_string() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => format!("{}.{}", d.as_secs(), d.subsec_nanos()),
        Err(_) => "0.0".to_owned(),
    }
}

/// Runs one full benchmark level at a specific concurrency and aggregates worker results.
async fn run_level(
    concurrency: usize,
    iters_per_worker: u64,
    payload: &[u8],
    tmp_base: &Path,
    workload: Workload,
    verify_inputs: Option<Arc<VerifyInputs>>,
) -> Result<LevelResult, WorkloadError> {
    let counter = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::with_capacity(concurrency);
    let level_start = Instant::now();

    for worker_id in 0..concurrency {
        let counter = Arc::clone(&counter);
        let payload = payload.to_vec();
        let tmp_base = tmp_base.to_path_buf();
        let verify_inputs = verify_inputs.clone();
        handles.push(tokio::spawn(async move {
            run_worker(
                worker_id,
                iters_per_worker,
                payload,
                tmp_base,
                counter,
                workload,
                verify_inputs,
            )
            .await
        }));
    }

    let mut successes = 0_u64;
    let mut failures = 0_u64;
    let mut latencies_ms = Vec::with_capacity((concurrency as u64 * iters_per_worker) as usize);
    for handle in handles {
        match handle.await {
            Ok(Ok(worker_result)) => {
                successes += worker_result.successes;
                failures += worker_result.failures;
                latencies_ms.extend(worker_result.latencies_ms);
            }
            Ok(Err(err)) => return Err(err),
            Err(err) => {
                return Err(WorkloadError::TaskJoin(err.to_string()));
            }
        }
    }

    let wall_time = level_start.elapsed();
    let throughput_ops_per_sec = if wall_time.as_secs_f64() > 0.0 {
        successes as f64 / wall_time.as_secs_f64()
    } else {
        0.0
    };

    let stats = calculate_latency_stats(&latencies_ms);
    Ok(LevelResult {
        concurrency,
        success_ops: successes,
        failures,
        wall_time,
        throughput_ops_per_sec,
        stats,
    })
}

struct WorkerResult {
    successes: u64,
    failures: u64,
    latencies_ms: Vec<f64>,
}

/// Executes `iters_per_worker` workload operations and records per-operation latency.
async fn run_worker(
    worker_id: usize,
    iters_per_worker: u64,
    payload: Vec<u8>,
    tmp_base: PathBuf,
    counter: Arc<AtomicU64>,
    workload: Workload,
    verify_inputs: Option<Arc<VerifyInputs>>,
) -> Result<WorkerResult, WorkloadError> {
    let mut successes = 0_u64;
    let mut failures = 0_u64;
    let mut latencies_ms = Vec::with_capacity(iters_per_worker as usize);
    let runtime_handle = Handle::current();

    for iter in 0..iters_per_worker {
        let payload = payload.clone();
        let tmp_base = tmp_base.clone();
        let counter = Arc::clone(&counter);
        let verify_inputs = verify_inputs.clone();
        let runtime_handle = runtime_handle.clone();
        let op_start = Instant::now();
        let op_result = tokio::task::spawn_blocking(move || match workload {
            Workload::Fs => run_fs_operation(&tmp_base, &payload, worker_id, iter, &counter)
                .map_err(WorkloadError::Fs),
            Workload::Tdx => {
                run_tdx_operation(worker_id, iter, &counter).map_err(WorkloadError::Tdx)
            }
            Workload::Verify => {
                let verify_inputs = verify_inputs.ok_or_else(|| {
                    WorkloadError::VerifyConfig(
                        "verification workload selected but verify inputs missing".to_owned(),
                    )
                })?;
                run_verify_operation(&runtime_handle, &verify_inputs).map_err(WorkloadError::Verify)
            }
        })
        .await
        .map_err(|err| WorkloadError::TaskJoin(err.to_string()))?;

        match op_result {
            Ok(()) => {
                successes += 1;
                latencies_ms.push(op_start.elapsed().as_secs_f64() * 1_000.0);
            }
            Err(err) => {
                if matches!(workload, Workload::Tdx | Workload::Verify) {
                    return Err(err);
                }
                failures += 1;
            }
        }
    }

    Ok(WorkerResult {
        successes,
        failures,
        latencies_ms,
    })
}

/// Performs one filesystem stand-in operation:
/// create, write, fsync, read, and delete a temporary file.
fn run_fs_operation(
    tmp_base: &Path,
    payload: &[u8],
    worker_id: usize,
    iter: u64,
    counter: &Arc<AtomicU64>,
) -> io::Result<()> {
    fs::create_dir_all(tmp_base)?;

    let unique = counter.fetch_add(1, AtomicOrdering::Relaxed);
    let file_path = tmp_base.join(format!(
        "dcap-generate-bench-w{worker_id}-i{iter}-u{unique}.tmp"
    ));

    let mut file = File::create(&file_path)?;
    file.write_all(payload)?;
    file.sync_all()?;
    drop(file);

    let mut contents = Vec::with_capacity(payload.len());
    let mut read_file = File::open(&file_path)?;
    read_file.read_to_end(&mut contents)?;
    fs::remove_file(&file_path)?;

    Ok(())
}

/// Performs one TDX quote generation operation with a unique 64-byte input.
fn run_tdx_operation(
    worker_id: usize,
    iter: u64,
    counter: &Arc<AtomicU64>,
) -> Result<(), QuoteGenerationError> {
    let input = make_tdx_input(worker_id, iter, counter);
    let _quote = configfs_tsm::create_tdx_quote(input)?;
    Ok(())
}

/// Performs one DCAP verification operation using preloaded test data.
fn run_verify_operation(
    runtime_handle: &Handle,
    verify_inputs: &VerifyInputs,
) -> Result<(), DcapVerificationError> {
    let quote_bytes = verify_inputs.quote_bytes.clone();
    let collateral = verify_inputs.collateral.clone();
    let expected_input_data = verify_inputs.expected_input_data;
    let now = verify_inputs.now;

    runtime_handle.block_on(async move {
        verify_dcap_attestation_with_given_timestamp(
            quote_bytes,
            expected_input_data,
            None,
            Some(collateral),
            now,
            false,
        )
        .await
        .map(|_| ())
    })
}

/// Builds a unique quote input to prevent concurrent input-name collisions in configfs-tsm.
fn make_tdx_input(worker_id: usize, iter: u64, counter: &Arc<AtomicU64>) -> [u8; 64] {
    let mut input = [0_u8; 64];
    let unique = counter.fetch_add(1, AtomicOrdering::Relaxed);
    input[0..8].copy_from_slice(&unique.to_le_bytes());
    input[8..16].copy_from_slice(&(worker_id as u64).to_le_bytes());
    input[16..24].copy_from_slice(&iter.to_le_bytes());
    input
}

/// Calculates latency summary statistics from operation samples in milliseconds.
fn calculate_latency_stats(samples_ms: &[f64]) -> Option<LatencyStats> {
    if samples_ms.is_empty() {
        return None;
    }

    let mut sorted = samples_ms.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let sum: f64 = sorted.iter().sum();
    let mean_ms = sum / sorted.len() as f64;

    Some(LatencyStats {
        mean_ms,
        p50_ms: percentile_nearest_rank(&sorted, 50),
        p95_ms: percentile_nearest_rank(&sorted, 95),
        p99_ms: percentile_nearest_rank(&sorted, 99),
        max_ms: *sorted.last().unwrap_or(&0.0),
    })
}

/// Returns the nearest-rank percentile value from sorted latency samples.
fn percentile_nearest_rank(sorted_samples: &[f64], percentile: usize) -> f64 {
    if sorted_samples.is_empty() {
        return f64::NAN;
    }
    let p = percentile.clamp(1, 100) as f64 / 100.0;
    let rank = (p * sorted_samples.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted_samples.len() - 1);
    sorted_samples[idx]
}

/// Prints a human-readable summary table for all benchmark levels.
fn print_table(results: &[LevelResult]) {
    println!(
        "{:<12} {:<10} {:<10} {:<14} {:<10} {:<10} {:<10} {:<10} {:<10}",
        "concurrency",
        "ops",
        "failures",
        "throughput/s",
        "mean_ms",
        "p50_ms",
        "p95_ms",
        "p99_ms",
        "max_ms"
    );

    for result in results {
        let (mean, p50, p95, p99, max) = match &result.stats {
            Some(stats) => (
                format!("{:.3}", stats.mean_ms),
                format!("{:.3}", stats.p50_ms),
                format!("{:.3}", stats.p95_ms),
                format!("{:.3}", stats.p99_ms),
                format!("{:.3}", stats.max_ms),
            ),
            None => (
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
            ),
        };

        println!(
            "{:<12} {:<10} {:<10} {:<14.3} {:<10} {:<10} {:<10} {:<10} {:<10}",
            result.concurrency,
            result.success_ops,
            result.failures,
            result.throughput_ops_per_sec,
            mean,
            p50,
            p95,
            p99,
            max
        );
    }
}

/// Returns a stable workload label for terminal and CSV output.
fn workload_name(workload: Workload) -> &'static str {
    match workload {
        Workload::Tdx => "tdx",
        Workload::Fs => "fs",
        Workload::Verify => "verify",
    }
}

/// Returns a benchmark label for the selected workload.
fn benchmark_name(workload: Workload) -> &'static str {
    match workload {
        Workload::Tdx => "tdx_quote",
        Workload::Fs => "fs_baseline",
        Workload::Verify => "verify_dcap",
    }
}

/// Loads and validates static verification inputs from the configured CLI paths.
fn load_verify_inputs(cli: &Cli) -> Result<VerifyInputs, String> {
    let quote_bytes =
        read_bytes_with_workspace_fallback(&cli.verify_quote_path).map_err(|err| {
            format!(
                "failed to read quote {}: {err}",
                cli.verify_quote_path.display()
            )
        })?;
    let collateral_bytes = read_bytes_with_workspace_fallback(&cli.verify_collateral_path)
        .map_err(|err| {
            format!(
                "failed to read collateral {}: {err}",
                cli.verify_collateral_path.display()
            )
        })?;
    let collateral: QuoteCollateralV3 = serde_json::from_slice(&collateral_bytes)
        .map_err(|err| format!("failed to parse collateral JSON: {err}"))?;
    let expected_input_data = parse_expected_input_hex(&cli.expected_input_hex)?;

    Ok(VerifyInputs {
        quote_bytes,
        collateral,
        expected_input_data,
        now: cli.verify_now,
    })
}

/// Reads bytes from the given path, with a workspace-root fallback for default relative paths.
fn read_bytes_with_workspace_fallback(path: &Path) -> io::Result<Vec<u8>> {
    if path.exists() || path.is_absolute() {
        return fs::read(path);
    }

    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "workspace root not found"))?;
    fs::read(workspace_root.join(path))
}

/// Parses a 64-byte hex string into an array for quote report-data comparison.
fn parse_expected_input_hex(value: &str) -> Result<[u8; 64], String> {
    if value.len() != 128 {
        return Err(format!(
            "must be exactly 128 hex chars (got {})",
            value.len()
        ));
    }
    if !value
        .as_bytes()
        .chunks_exact(2)
        .all(|pair| pair.iter().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("must contain only hex characters".to_owned());
    }

    let mut out = [0_u8; 64];
    for (idx, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let hex_pair =
            std::str::from_utf8(pair).map_err(|_| "contains invalid UTF-8".to_owned())?;
        out[idx] = u8::from_str_radix(hex_pair, 16)
            .map_err(|err| format!("contains invalid hex: {err}"))?;
    }
    Ok(out)
}

/// Writes benchmark results to CSV using one row per concurrency level.
fn write_csv(
    csv_path: &Path,
    timestamp: &str,
    workload: Workload,
    payload_bytes: usize,
    iters_per_worker: u64,
    results: &[LevelResult],
) -> io::Result<()> {
    let file = File::create(csv_path)?;
    let mut writer = BufWriter::new(file);

    writeln!(
        writer,
        "timestamp,benchmark,workload,concurrency,ops,failures,wall_time_ms,throughput_ops_s,mean_ms,p50_ms,p95_ms,p99_ms,max_ms,payload_bytes,iters_per_worker"
    )?;

    for result in results {
        let (mean, p50, p95, p99, max) = match &result.stats {
            Some(stats) => (
                format!("{:.6}", stats.mean_ms),
                format!("{:.6}", stats.p50_ms),
                format!("{:.6}", stats.p95_ms),
                format!("{:.6}", stats.p99_ms),
                format!("{:.6}", stats.max_ms),
            ),
            None => (
                "NaN".to_owned(),
                "NaN".to_owned(),
                "NaN".to_owned(),
                "NaN".to_owned(),
                "NaN".to_owned(),
            ),
        };

        writeln!(
            writer,
            "{},{},{},{},{},{},{:.3},{:.6},{},{},{},{},{},{},{}",
            timestamp,
            benchmark_name(workload),
            workload_name(workload),
            result.concurrency,
            result.success_ops,
            result.failures,
            result.wall_time.as_secs_f64() * 1_000.0,
            result.throughput_ops_per_sec,
            mean,
            p50,
            p95,
            p99,
            max,
            payload_bytes,
            iters_per_worker
        )?;
    }

    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use super::{
        Cli, Workload, calculate_latency_stats, make_tdx_input, parse_expected_input_hex,
        percentile_nearest_rank,
    };

    #[test]
    fn percentile_uses_nearest_rank() {
        let samples = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile_nearest_rank(&samples, 50), 3.0);
        assert_eq!(percentile_nearest_rank(&samples, 95), 5.0);
        assert_eq!(percentile_nearest_rank(&samples, 99), 5.0);
    }

    #[test]
    fn calculate_stats_returns_expected_values() {
        let samples = vec![1.0, 3.0, 2.0, 4.0];
        let stats = calculate_latency_stats(&samples).expect("stats should exist");
        assert!((stats.mean_ms - 2.5).abs() < 1e-9);
        assert_eq!(stats.p50_ms, 2.0);
        assert_eq!(stats.p95_ms, 4.0);
        assert_eq!(stats.p99_ms, 4.0);
        assert_eq!(stats.max_ms, 4.0);
    }

    #[test]
    fn calculate_stats_handles_empty_samples() {
        assert!(calculate_latency_stats(&[]).is_none());
    }

    #[test]
    fn cli_parses_workload_values() {
        let tdx = Cli::try_parse_from(["bin", "--workload", "tdx"]).expect("tdx should parse");
        assert_eq!(tdx.workload, Workload::Tdx);

        let fs = Cli::try_parse_from(["bin", "--workload", "fs"]).expect("fs should parse");
        assert_eq!(fs.workload, Workload::Fs);

        let verify =
            Cli::try_parse_from(["bin", "--workload", "verify"]).expect("verify should parse");
        assert_eq!(verify.workload, Workload::Verify);
    }

    #[test]
    fn parse_expected_input_hex_accepts_valid_input() {
        let parsed = parse_expected_input_hex(super::DEFAULT_EXPECTED_INPUT_HEX)
            .expect("default expected input should parse");
        assert_eq!(parsed.len(), 64);
    }

    #[test]
    fn parse_expected_input_hex_rejects_wrong_length() {
        let err = parse_expected_input_hex("abcd").expect_err("short input should fail");
        assert!(err.contains("128"));
    }

    #[test]
    fn parse_expected_input_hex_rejects_non_hex() {
        let mut invalid = "00".repeat(64);
        invalid.replace_range(0..2, "zz");
        let err = parse_expected_input_hex(&invalid).expect_err("non-hex should fail");
        assert!(err.contains("hex"));
    }

    #[test]
    fn tdx_input_is_unique_per_call() {
        let counter = Arc::new(AtomicU64::new(0));
        let first = make_tdx_input(0, 0, &counter);
        let second = make_tdx_input(0, 0, &counter);
        assert_ne!(first, second);
        assert_eq!(first.len(), 64);
        assert_eq!(second.len(), 64);
    }
}
