use std::fs;

use assert_cmd::cargo::cargo_bin_cmd;

#[test]
fn benchmark_smoke_writes_csv() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let csv_path = tmp.path().join("smoke.csv");

    cargo_bin_cmd!("dcap-generate-benchmark")
        .args([
            "--workload",
            "fs",
            "--concurrency",
            "1,2",
            "--iters-per-worker",
            "2",
            "--payload-bytes",
            "16",
            "--tmp-dir",
            tmp.path().to_str().expect("tmp dir should be utf8"),
            "--csv-path",
            csv_path.to_str().expect("csv path should be utf8"),
        ])
        .assert()
        .success();

    let csv = fs::read_to_string(&csv_path).expect("csv should exist");
    assert!(csv.starts_with("timestamp,benchmark,workload,concurrency,ops,failures"));
    assert!(csv.contains("fs_baseline"));
}
