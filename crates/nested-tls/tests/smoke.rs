use std::fs;

use assert_cmd::prelude::*;
use std::process::Command;

#[test]
fn benchmark_smoke_writes_csv_for_both_modes() {
    let tmp = tempfile::tempdir().expect("tempdir should be created");
    let csv_path = tmp.path().join("smoke.csv");

    let mut cmd = Command::new("cargo");
    cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["run", "-p", "nested-tls-benchmark", "--"])
        .args([
            "--modes",
            "single-tls,nested-tls",
            "--payload-sizes",
            "4096",
            "--chunk-bytes",
            "1024",
            "--rounds",
            "2",
            "--csv-path",
            csv_path.to_str().expect("csv path should be utf8"),
        ]);

    cmd.assert().success();

    let csv = fs::read_to_string(&csv_path).expect("csv should exist");
    assert!(csv.starts_with("timestamp,benchmark,mode,payload_bytes,chunk_bytes,rounds"));
    assert!(csv.contains("single_tls"));
    assert!(csv.contains("nested_tls"));
}
