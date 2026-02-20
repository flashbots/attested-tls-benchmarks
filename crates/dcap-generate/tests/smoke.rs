use std::fs;

use assert_cmd::Command;

#[test]
fn benchmark_smoke_writes_csv() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let csv_path = tmp.path().join("smoke.csv");

    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root should exist");

    Command::new("cargo")
        .current_dir(workspace_root)
        .args([
            "run",
            "-p",
            "dcap-generate-benchmark",
            "--",
        ])
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
