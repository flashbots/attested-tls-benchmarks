# attested-tls-benchmarks

Small Rust workspace for benchmarking components related to attested TLS.

This repo will contain multiple benchmarking programs over time.

Currently it includes:
- `crates/dcap-generate`: benchmark harness for DCAP quote generation workflows (with an FS baseline mode and a TDX quote mode).

## Results

### DCAP Generation on GCP

```
concurrency  ops        failures   throughput/s   mean_ms    p50_ms     p95_ms     p99_ms     max_ms
1            200        0          25.588         39.078     38.946     40.276     40.748     41.100
2            400        0          25.680         77.684     77.674     81.223     116.981    117.513
4            800        0          25.609         154.951    155.651    200.289    239.039    272.946
8            1600       0          25.519         309.751    351.501    430.237    469.886    521.425
16           3200       0          25.477         612.495    743.713    899.543    940.691    1083.260
32           6400       0          25.332         1223.761   1575.090   1846.041   1932.947   2039.212
64           12800      0          25.171         2473.967   3251.411   3598.749   3715.820   3935.253
128          25600      0          24.769         5020.951   6807.242   7311.143   7586.950   7971.502
```
