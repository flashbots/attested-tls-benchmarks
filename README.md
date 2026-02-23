# attested-tls-benchmarks

Small Rust workspace for benchmarking components related to attested TLS.

This repo will contain multiple benchmarking programs over time.

Currently it includes:
- `crates/dcap-generate`: benchmark harness for DCAP quote generation workflows (with an FS baseline mode and a TDX quote mode).
  - Also supports a `verify` workload for benchmarking offline DCAP verification using local quote/collateral test vectors.

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

![Chart showing quote generation serialization](./results-gcp/quote_serialization.svg)

Basically this shows us that attestation generation takes around 39ms (which is longer than i expected). And it is highly serialized - two concurrent generations take almost exactly twice long as one generation.

### DCAP Verification (offline test vectors)

Benchmark attestation verification with fixed local test data and a deterministic timestamp:

```bash
cargo run -p dcap-generate-benchmark -- \
  --workload verify \
  --concurrency 1,2,4,8 \
  --iters-per-worker 50 \
  --verify-now 1769509141
```

Defaults for `verify` workload:
- Quote: `crates/dcap-generate/test-data/dcap-tdx-1766059550570652607`
- Collateral: `crates/dcap-generate/test-data/dcap-quote-collateral-00.json`
- Expected report data:
  `74276a648f1fd491f474a2d52c72d850e3768157b43ec297a9917482bd77278ba1882588391d1956b6f6466ad8b8dccd55f57221ad81b420f746fa8db0f8637d`
- Timestamp: `1769509141`

On peg's computer (24 cores AMD Ryzen AI 9 HX 370) **with pre-fetched in-memory collateral**:

```
concurrency  ops        failures   throughput/s   mean_ms    p50_ms     p95_ms     p99_ms     max_ms
1            200        0          1861.895       0.536      0.509      0.595      1.105      1.411
2            400        0          2357.684       0.840      1.077      1.165      1.253      1.570
4            800        0          4953.148       0.780      0.785      1.111      1.543      1.664
8            1600       0          10189.188      0.764      0.785      0.948      1.285      1.658
16           3200       0          14119.984      1.103      1.072      1.448      1.503      2.036
32           6400       0          15239.759      2.035      1.681      3.840      4.971      7.894
64           12800      0          15211.749      4.060      3.193      9.662      13.761     27.784
128          25600      0          15018.145      8.197      6.910      19.117     27.636     49.533
```

![Chart showing quote verification serialization](./results-verify-with-collateral/1771832655_872976764-verify_dcap.svg)

With pre-loaded collateral, verification is fast (0.5ms) and can be parallelized - throughput saturates around ~15k ops/s at roughly 32 concurrency on my machine.
