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

The exe carries its own icon and version info (`assets/icon.svg` is the source, `assets/icon.ico` is embedded by
`build.rs`; the resource compiler `rc.exe` comes with the Windows SDK that the Rust MSVC toolchain needs anyway, and
without it the build simply continues without the icon).

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

## Run all tests

**▶ Run all tests…** in the title bar opens the plan on the Dashboard; **▶ Run all tests** starts it. The app then

1. collects the system data and waits 10 s (a prompt tells you the first test needs you),
2. runs the **mouse-click and key-press trials first**: wait for the screen to turn green, click / press (10 trials
   each, about a minute). Everything after that runs by itself, so you can walk away. *Skip the click / key tests* in the
   bar above skips them,
3. runs the input timing tests (timer, jitter, polling; unpinned and on every core), then **memory** (all 18 buffer
   sizes, all 14 access patterns, every thread count up to your logical CPUs, 10 runs, 1 warm-up run, 5 s limit per
   test), **CPU** (all 12 workloads on all cores, 10 s runs × 10 with 10 s warm-up, then each core on its own) and
   **GPU** (six sizes, each dispatched for at least 2 s so the load registers),
4. signs and saves one record (`latency_results/…_session_….json`), writes a folder
   `latency_results/run_all_<date_time>/` with `memory.csv`, `cpu.csv`, `gpu.csv`, `input_timing.csv`,
   `input_trials.csv`, `sensors.csv`, a self-contained `report.html` and a short `summary.txt`, and shows everything in
   **Results & Graphs**.

The full profile takes **hours** (the panel shows a typical and a worst-case time before you start; the per-core CPU
test alone is one run per workload per core). Use **Quick check** for a few-minute pass, untick parts, or change any
number in the panel. *Every core on its own* for memory is off by default because it adds thousands of tests. **Stop**
ends the run early and still saves and exports what finished; a step that fails (for example no Vulkan device) is
noted and the run carries on.

The manual tabs use the same defaults (memory: 10 runs, 1 warm-up, 5 s limit; CPU: 10 s runs × 10, 10 s warm-up, all
workloads), and every suite can still be run and saved by hand.

## Choosing what to test

Every tab shows live progress (what is running right now, ETA) and fills its results in as tests finish;
**Stop** keeps whatever already finished.

- **Cores** (Memory, CPU, Input): all / P-cores / E-cores / P+E pinned / any specific cores, plus
  **Test each core on its own** to find slow, hot or faulty cores. The Results tab flags cores that are
  well below the median.
- **Memory**: pick buffer sizes, access patterns, thread counts (presets or a custom number), runs per
  test and a time limit per test.
- **CPU**: pick workloads, thread count (all selected cores or an exact number), run length and repeats.

## Sensors and graphs

While any test runs, a background sampler records CPU per-core temperatures, clocks, load, RAM and
GPU temperature/load/power/VRAM, and each result stores a summary of the readings taken during that test.
The **Results & Graphs** tab charts every value, and each line or column can be switched on and off.

| Reading | Linux | Windows |
|---|---|---|
| CPU temperature (package / per core) | hwmon (`coretemp`, `k10temp`) | per-core needs [LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor) (or OpenHardwareMonitor) running; otherwise a package-level ACPI thermal zone |
| CPU clocks / load / RAM | yes | yes |
| GPU temp / load / power / VRAM | `nvidia-smi` or amdgpu sysfs | `nvidia-smi` (ships with the NVIDIA driver) |

The Dashboard lists which sensors were found. Windows itself does not expose per-core CPU temperatures
to normal programs, so without a helper such as LibreHardwareMonitor those columns stay empty.

Notes on the numbers:

- The ACPI thermal-zone fallback is not a CPU sensor on every PC: many boards report a fixed value (or a
  whole-degree value that rarely changes). When the app sees a CPU temperature that has not moved for
  ~15 s while the CPU is busy it says so in the sensor notes; run LibreHardwareMonitor for real readings.
  Of several ACPI zones, the one that actually changes is used rather than simply the hottest.
- If the Windows sensor helper stops updating, its old readings are discarded (and it is restarted)
  instead of being repeated as if they were live.
- Each GPU size keeps dispatching for at least 2 s so GPU load, clocks, power and temperature have time to
  register; a single dispatch takes microseconds and would read as ~0 % load. GPU load comes from
  `nvidia-smi` or, on Linux/AMD, `gpu_busy_percent`.

## Logging and viewing results

- **Log everything** (Dashboard) runs nothing new: it saves all results currently in the app (memory, CPU, GPU,
  input, per-core sweeps, sensor timeline, full system/hardware info) as **one signed session file**. Each suite
  also has its own "Log …" button.
- The **log panel** at the bottom of the window is a normal read-only text box: drag to select, `Ctrl+A` to
  select all, `Ctrl+C` to copy, or use **Copy all / Save… / Clear**.
- **Web viewer**: in the app press *Start server & open browser*, or run

  ```sh
  latency-tester --serve [--dir ./latency_results] [--port 8080] [--bind 127.0.0.1] [--open]
  latency-tester --report [--dir ./latency_results] [--out report.html] [file.json ...]
  ```

  `--serve` is a read-only local web server (no uploads, only the result `*.json` files of one folder;
  bind to `0.0.0.0` only if you want other devices on your network to see it). `--report` writes one
  self-contained `report.html` (no external resources: e-mail it or open it offline). The page shows each
  file's signature status (valid / edited / signed by another key), the system info, per-test results, sweeps
  and the sensor timeline, with per-series toggles.

## Automated input-latency rig (optional)

The Input tab has separate **mouse click** and **keyboard press** tests (10 trials averaged, random 500–2000 ms
waits so you cannot anticipate, red *wait* / green *go* screen with a black/white marker bar and a
red-black-red finish flash). A small Arduino Pro Micro rig can click/press for you, and measure the
display too (refresh rate, response time, click-to-photon). Schematics, parts list, wiring and calibration:
[`docs/latency-rig/README.md`](docs/latency-rig/README.md). Firmware: [`arduino/latency_rig`](arduino/latency_rig).

## Tests

```sh
cargo test
g++ -std=c++11 -o /tmp/t arduino/tests/test_analysis.cpp && /tmp/t   # rig firmware maths, see arduino/tests/README.md
```

GPU tests skip themselves when no Vulkan device is available.
