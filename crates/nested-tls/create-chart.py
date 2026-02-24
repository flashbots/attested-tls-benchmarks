#!/usr/bin/env python3
import argparse
from pathlib import Path

import matplotlib.pyplot as plt
import pandas as pd


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Create throughput/CPU/overhead charts from nested TLS benchmark CSV output."
    )
    parser.add_argument("csv_path", help="Path to benchmark CSV")
    parser.add_argument(
        "--concurrency",
        type=int,
        default=1,
        help="Concurrency value to chart (default: 1)",
    )
    parser.add_argument(
        "--output",
        default=None,
        help="Output image path (default: <csv_basename>-chart.png)",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    csv_path = Path(args.csv_path)
    if not csv_path.exists():
        raise FileNotFoundError(f"CSV not found: {csv_path}")

    df = pd.read_csv(csv_path)
    needed = {
        "mode",
        "payload_bytes",
        "concurrency",
        "throughput_mib_s_mean",
        "cpu_ms_per_mib",
        "overhead_vs_single_pct",
    }
    missing = needed.difference(df.columns)
    if missing:
        raise ValueError(f"CSV missing required columns: {sorted(missing)}")

    df = df[df["concurrency"] == args.concurrency].copy()
    if df.empty:
        raise ValueError(f"No rows found for concurrency={args.concurrency}")

    df["payload_mib"] = df["payload_bytes"] / (1024 * 1024)
    single = df[df["mode"] == "single_tls"].sort_values("payload_bytes")
    nested = df[df["mode"] == "nested_tls"].sort_values("payload_bytes")

    fig, ax = plt.subplots(1, 3, figsize=(14, 4))

    ax[0].plot(
        single["payload_mib"],
        single["throughput_mib_s_mean"],
        marker="o",
        label="single_tls",
    )
    ax[0].plot(
        nested["payload_mib"],
        nested["throughput_mib_s_mean"],
        marker="o",
        label="nested_tls",
    )
    ax[0].set_xscale("log", base=2)
    ax[0].set_title("Throughput")
    ax[0].set_xlabel("Payload (MiB)")
    ax[0].set_ylabel("MiB/s")
    ax[0].legend()
    ax[0].grid(True, alpha=0.3)

    ax[1].plot(
        single["payload_mib"], single["cpu_ms_per_mib"], marker="o", label="single_tls"
    )
    ax[1].plot(
        nested["payload_mib"], nested["cpu_ms_per_mib"], marker="o", label="nested_tls"
    )
    ax[1].set_xscale("log", base=2)
    ax[1].set_title("CPU Cost")
    ax[1].set_xlabel("Payload (MiB)")
    ax[1].set_ylabel("CPU ms / MiB")
    ax[1].legend()
    ax[1].grid(True, alpha=0.3)

    ax[2].plot(
        nested["payload_mib"],
        nested["overhead_vs_single_pct"],
        marker="o",
        color="crimson",
    )
    ax[2].set_xscale("log", base=2)
    ax[2].set_title("Nested Overhead")
    ax[2].set_xlabel("Payload (MiB)")
    ax[2].set_ylabel("Overhead %")
    ax[2].grid(True, alpha=0.3)

    fig.suptitle(f"Nested TLS Benchmark (concurrency={args.concurrency})", y=1.03)
    fig.tight_layout()

    output = Path(args.output) if args.output else csv_path.with_name(f"{csv_path.stem}-chart.png")
    fig.savefig(output, dpi=180, bbox_inches="tight")
    print(f"wrote {output}")


if __name__ == "__main__":
    main()
