import argparse

import matplotlib.pyplot as plt
import pandas as pd


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Create throughput/latency charts from benchmark CSV output."
    )
    parser.add_argument("csv_path", help="Path to benchmark CSV (for example results/*.csv)")
    parser.add_argument(
        "--out-prefix",
        default="quote_serialization",
        help="Output file prefix without extension (default: quote_serialization)",
    )
    args = parser.parse_args()

    df = pd.read_csv(args.csv_path).sort_values("concurrency")
    base = df.loc[df["concurrency"] == 1, "throughput_ops_s"].iloc[0]
    df["ideal"] = base * df["concurrency"]

    benchmark = str(df["benchmark"].iloc[0]) if "benchmark" in df.columns else "benchmark"
    workload = str(df["workload"].iloc[0]) if "workload" in df.columns else "workload"

    fig, ax = plt.subplots(1, 2, figsize=(12, 4))

    ax[0].plot(df["concurrency"], df["throughput_ops_s"], marker="o", label="measured")
    ax[0].plot(df["concurrency"], df["ideal"], "--", label="ideal linear")
    ax[0].set_xscale("log", base=2)
    ax[0].set_title(f"Throughput vs Concurrency ({benchmark})")
    ax[0].set_xlabel("Concurrency")
    ax[0].set_ylabel("Ops/s")
    ax[0].legend()

    for col in ["p50_ms", "p95_ms", "p99_ms"]:
        ax[1].plot(df["concurrency"], df[col], marker="o", label=col)
    ax[1].set_xscale("log", base=2)
    ax[1].set_title(f"Latency vs Concurrency ({workload})")
    ax[1].set_xlabel("Concurrency")
    ax[1].set_ylabel("Latency (ms)")
    ax[1].legend()

    plt.tight_layout()
    plt.savefig(f"{args.out_prefix}.png", dpi=200)
    plt.savefig(f"{args.out_prefix}.svg")


if __name__ == "__main__":
    main()
