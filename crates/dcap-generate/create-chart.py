import pandas as pd
import matplotlib.pyplot as plt

df = pd.read_csv("../../results-gcp/1771605431_640795338-fs-baseline.csv").sort_values("concurrency")
base = df.loc[df["concurrency"] == 1, "throughput_ops_s"].iloc[0]
df["ideal"] = base * df["concurrency"]

fig, ax = plt.subplots(1, 2, figsize=(12,4))

ax[0].plot(df["concurrency"], df["throughput_ops_s"], marker="o", label="measured")
ax[0].plot(df["concurrency"], df["ideal"], "--", label="ideal linear")
ax[0].set_xscale("log", base=2)
ax[0].set_title("Throughput vs Concurrency")
ax[0].set_xlabel("Concurrency")
ax[0].set_ylabel("Ops/s")
ax[0].legend()

for col in ["p50_ms", "p95_ms", "p99_ms"]:
    ax[1].plot(df["concurrency"], df[col], marker="o", label=col)
ax[1].set_xscale("log", base=2)
ax[1].set_title("Latency vs Concurrency")
ax[1].set_xlabel("Concurrency")
ax[1].set_ylabel("Latency (ms)")
ax[1].legend()

plt.tight_layout()
plt.savefig("quote_serialization.png", dpi=200)   # PNG image file
plt.savefig("quote_serialization.svg")            # vector image for slides/docs
