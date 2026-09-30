# Latency Tester Suite

Cross-platform (Windows / Linux) latency benchmarks written in Rust, with an egui GUI and a headless CLI.

| Suite  | What it measures |
|--------|------------------|
| Memory | Sequential / random / strided / pointer-chase / stream (copy, scale, add, triad) access, 1–N threads, latency + bandwidth + percentiles |
| CPU    | Integer, float, vector, crypto-like, mixed, compile-sim and game-sim workloads with core affinity (all / P-cores / E-cores / single / HT pairs) |
| GPU    | Vulkan compute dispatch latency and throughput. The SPIR-V shader is generated at run time and its output is verified against a CPU reference |
| Input  | Interactive click-reaction test (GUI), plus timer resolution, timing jitter and loop-polling checks. The headless input checks measure the OS timing/scheduling stack, not real hardware |

Results can be signed (HMAC-SHA256 over canonical JSON), verified, exported to CSV, and bundled into signed packages.

## Build & run

```sh
cargo build --release
./target/release/latency-tester                 # GUI
./target/release/latency-tester --cli           # quick headless pass of all suites
./target/release/latency-tester --cli --skip gpu --out ./my_results
```

Linux GUI needs `libxkbcommon`, X11/Wayland and OpenGL. The GPU suite needs a Vulkan driver
(software drivers such as Mesa lavapipe work: `VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json`).

## Tests

```sh
cargo test
```

GPU tests skip themselves when no Vulkan device is available.
