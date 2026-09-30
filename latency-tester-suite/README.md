# Latency Tester Suite

Cross-platform (Windows / Linux) latency benchmarks written in Rust, with an egui GUI and a headless CLI.

| Suite  | What it measures |
|--------|------------------|
| Memory | Sequential / random / strided / pointer-chase / stream (copy, scale, add, triad) access, 1–N threads, latency + bandwidth + percentiles |
| CPU    | Integer, float, vector, crypto-like, mixed, compile-sim and game-sim workloads with core affinity (all / P-cores / E-cores / single / HT pairs) |
| GPU    | Vulkan compute dispatch latency and throughput. The SPIR-V shader is generated at run time and its output is verified against a CPU reference |
| Input  | Interactive click-reaction test (GUI), plus timer resolution, timing jitter and loop-polling checks. The headless input checks measure the OS timing/scheduling stack, not real hardware |

Results are signed (Ed25519 over canonical JSON), verified, exported to CSV, and bundled into signed packages.
Editing is detectable and discouraged at several levels:

- Every record carries an Ed25519 signature; anyone can verify it with just the public key (`--cli` prints it).
- Records are hash-chained, so editing, deleting, inserting or reordering entries in the master log fails "Verify Entire Log".
- Saved result files and the signing key are written read-only (key `0400` on Unix).

Limit: the signing key lives on the machine that ran the test, so someone with full access to that machine
could still generate a new key and re-sign. Verify against a public key you recorded earlier or received
separately; stronger guarantees would need an external timestamping/signing service.

## Build & run

One step (needs [Rust](https://rustup.rs)):

| | Build | Run the GUI |
|---|---|---|
| Windows | `build.bat` → `dist\LatencyTester.exe` | double-click the exe, or `run.bat` |
| Linux   | `./build.sh` → `dist/latency-tester` | `./run.sh` |

The exe is self-contained: the MSVC runtime is linked statically, the Vulkan loader is
optional (loaded at run time; the GPU suite reports "no device" without it), and results are
written to a `latency_results` folder next to the exe. Copy the single file anywhere to use it.

Command line (also works on the built exe):

```sh
latency-tester --cli                       # quick headless pass of all suites
latency-tester --cli --skip gpu --out ./my_results
```

Linux GUI needs `libxkbcommon`, X11/Wayland and OpenGL. The GPU suite needs a Vulkan driver
(software drivers such as Mesa lavapipe work: `VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json`).

## Tests

```sh
cargo test
```

GPU tests skip themselves when no Vulkan device is available.
