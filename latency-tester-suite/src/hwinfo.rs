#![allow(dead_code)] // some parsers are only used on one OS but are tested everywhere
//! Hardware / OS details that `sysinfo` does not provide: OS edition and build, motherboard and BIOS,
//! RAM modules, GPUs and CPU caches. Everything works without administrator rights.
//!
//! * Windows: one PowerShell/CIM query (memory modules, video controllers, OS, BIOS/board registry)
//! * Linux: `/sys/class/dmi/id`, `/etc/os-release`, `dmidecode` when it happens to be runnable
//! * NVIDIA GPUs (both): `nvidia-smi`;  AMD on Linux: amdgpu sysfs
//! * CPU caches: CPUID (leaf 4 / 0x8000001D)
//!
//! The parsers are pure functions so they can be unit tested with captured output.

use serde::Deserialize;
use std::path::Path;

use crate::system_info::{GpuInfo, MemoryModule, MemoryTimings, MotherboardInfo, OsInfo};

#[derive(Debug, Clone, Default)]
pub struct HwExtras {
    pub os: Option<OsInfo>,
    pub board: Option<MotherboardInfo>,
    pub modules: Vec<MemoryModule>,
    pub memory_type: Option<String>,
    pub gpus: Vec<GpuInfo>,
}

fn unknown(s: &str) -> bool {
    let t = s.trim();
    t.is_empty() || t.eq_ignore_ascii_case("unknown") || t.eq_ignore_ascii_case("to be filled by o.e.m.") || t.eq_ignore_ascii_case("default string") || t.eq_ignore_ascii_case("system product name") || t.eq_ignore_ascii_case("system manufacturer") || t.eq_ignore_ascii_case("none") || t == "N/A"
}

fn or_unknown(s: Option<String>) -> String {
    match s {
        Some(v) if !unknown(&v) => v.trim().to_string(),
        _ => "Unknown".to_string(),
    }
}

/// "Z890" from "TUF GAMING Z890-PLUS WIFI", "B650" from "ROG STRIX B650E-F"... (inferred, not read from hardware)
pub fn guess_chipset(board: &str) -> Option<String> {
    let up = board.to_uppercase();
    for token in up.split(|c: char| !c.is_ascii_alphanumeric()) {
        let b = token.as_bytes();
        if b.len() >= 4 && b.len() <= 5 && b[0].is_ascii_alphabetic() && b[1..4].iter().all(|c| c.is_ascii_digit()) {
            let known = matches!(b[0], b'Z' | b'B' | b'H' | b'Q' | b'W' | b'X' | b'A' | b'T');
            let suffix_ok = b.len() == 4 || b[4].is_ascii_alphabetic();
            if known && suffix_ok {
                return Some(token.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Pure parsers
// ---------------------------------------------------------------------------------------------

#[allow(dead_code)]
/// Value of `name` in `reg query` output:  "    ProductName    REG_SZ    Windows 10 Pro"
pub fn parse_reg_value(output: &str, name: &str) -> Option<String> {
    for line in output.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix(name) {
            if rest.starts_with(char::is_whitespace) {
                let mut parts = rest.trim_start().splitn(2, char::is_whitespace);
                let _ty = parts.next()?;
                return parts.next().map(|v| v.trim().to_string());
            }
        }
    }
    None
}

/// KEY="value" lines of /etc/os-release
pub fn parse_os_release(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.trim().trim_matches('"').to_string())
    })
}

/// Read `/sys/class/dmi/id`-style files
pub fn read_dmi(root: &Path) -> (MotherboardInfo, Option<String>) {
    let rd = |f: &str| std::fs::read_to_string(root.join(f)).ok().map(|s| s.trim().to_string());
    let model = or_unknown(rd("board_name"));
    let board = MotherboardInfo {
        manufacturer: or_unknown(rd("board_vendor")),
        chipset: guess_chipset(&model).map(|c| format!("{} (from board name)", c)).unwrap_or_else(|| "Unknown".into()),
        model,
        version: or_unknown(rd("board_version")),
        bios_version: or_unknown(rd("bios_version")),
        bios_date: or_unknown(rd("bios_date")),
    };
    (board, rd("product_name").filter(|p| !unknown(p)))
}

pub fn memory_type_name(smbios: i64) -> Option<&'static str> {
    Some(match smbios {
        20 => "DDR",
        21 => "DDR2",
        24 => "DDR3",
        26 => "DDR4",
        27 => "LPDDR",
        28 => "LPDDR2",
        29 => "LPDDR3",
        30 => "LPDDR4",
        34 => "DDR5",
        35 => "LPDDR5",
        _ => return None,
    })
}

/// dmidecode "Memory Device" blocks -> modules and the DIMM type
pub fn parse_dmidecode_memory(text: &str) -> (Vec<MemoryModule>, Option<String>) {
    let mut modules = Vec::new();
    let mut mtype = None;
    for block in text.split("\n\n") {
        if !block.contains("Memory Device") {
            continue;
        }
        let get = |key: &str| -> Option<String> {
            block.lines().find_map(|l| {
                let (k, v) = l.trim().split_once(':')?;
                (k.trim() == key).then(|| v.trim().to_string())
            })
        };
        let size = get("Size").unwrap_or_default();
        let bytes = {
            let mut it = size.split_whitespace();
            match (it.next().and_then(|n| n.parse::<u64>().ok()), it.next()) {
                (Some(n), Some("GB")) => n << 30,
                (Some(n), Some("MB")) => n << 20,
                _ => 0,
            }
        };
        if bytes == 0 {
            continue; // empty slot
        }
        let speed = |s: Option<String>| -> u32 { s.and_then(|v| v.split_whitespace().next().and_then(|n| n.parse().ok())).unwrap_or(0) };
        let configured = speed(get("Configured Memory Speed").or_else(|| get("Configured Clock Speed")));
        let rated = speed(get("Speed"));
        if let Some(t) = get("Type").filter(|t| t.starts_with("DDR") || t.starts_with("LPDDR")) {
            mtype = Some(t);
        }
        modules.push(MemoryModule {
            size: bytes,
            speed: if configured > 0 { configured } else { rated },
            manufacturer: or_unknown(get("Manufacturer")),
            part_number: or_unknown(get("Part Number")),
            serial_number: or_unknown(get("Serial Number")),
            timings: MemoryTimings { cl: 0, trcd: 0, trp: 0, tras: 0, trc: 0, voltage: get("Configured Voltage").and_then(|v| v.split_whitespace().next().and_then(|n| n.parse().ok())).unwrap_or(0.0) },
        });
    }
    (modules, mtype)
}

/// One row of `nvidia-smi --query-gpu=name,driver_version,memory.total,clocks.max.gr,clocks.max.mem,temperature.gpu,power.draw,fan.speed`
pub fn parse_nvidia_smi_info(out: &str) -> Vec<GpuInfo> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            if f.len() < 8 {
                return None;
            }
            let num = |s: &str| s.parse::<f64>().ok();
            Some(GpuInfo {
                name: f[0].to_string(),
                vendor: "NVIDIA".into(),
                vram_total: num(f[2]).map(|mb| (mb * 1048576.0) as u64).unwrap_or(0),
                vram_type: "Unknown".into(),
                vram_bus_width: 0,
                core_clock: num(f[3]).unwrap_or(0.0) as u32,
                memory_clock: num(f[4]).unwrap_or(0.0) as u32,
                compute_units: 0,
                driver_version: f[1].to_string(),
                api_version: "Unknown".into(),
                temperature: num(f[5]).unwrap_or(0.0) as f32,
                power_watts: num(f[6]).unwrap_or(0.0) as f32,
                fan_speed: num(f[7]).unwrap_or(0.0) as u32,
            })
        })
        .collect()
}

/// JSON printed by [`WIN_HW_SCRIPT`]
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct WinHw {
    mem: serde_json::Value,
    gpu: serde_json::Value,
    os: serde_json::Value,
    bios: serde_json::Value,
}

fn as_list(v: &serde_json::Value) -> Vec<serde_json::Value> {
    match v {
        serde_json::Value::Array(a) => a.clone(),
        serde_json::Value::Null => Vec::new(),
        other => vec![other.clone()],
    }
}

fn js(v: &serde_json::Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// Parse the Windows PowerShell hardware query
pub fn parse_win_hw(json: &str) -> Option<HwExtras> {
    let start = json.find('{')?;
    let hw: WinHw = serde_json::from_str(json[start..].trim()).ok()?;
    let mut out = HwExtras::default();

    for m in as_list(&hw.mem) {
        let cap = m["cap"].as_f64().unwrap_or(0.0) as u64;
        if cap == 0 {
            continue;
        }
        let cfg = m["cfg"].as_i64().unwrap_or(0);
        let spd = m["spd"].as_i64().unwrap_or(0);
        if out.memory_type.is_none() {
            out.memory_type = m["t"].as_i64().and_then(memory_type_name).map(String::from);
        }
        out.modules.push(MemoryModule {
            size: cap,
            speed: if cfg > 0 { cfg as u32 } else { spd as u32 },
            manufacturer: or_unknown(js(&m, "mfr")),
            part_number: or_unknown(js(&m, "pn")),
            serial_number: or_unknown(js(&m, "sn")),
            timings: MemoryTimings { cl: 0, trcd: 0, trp: 0, tras: 0, trc: 0, voltage: 0.0 },
        });
    }

    for g in as_list(&hw.gpu) {
        let name = js(&g, "n").unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        out.gpus.push(GpuInfo {
            vendor: js(&g, "v").filter(|v| !unknown(v)).unwrap_or_else(|| "Unknown".into()),
            name,
            // AdapterRAM is a 32-bit field and wraps at 4 GB; nvidia-smi supplies the real value
            vram_total: g["r"].as_f64().unwrap_or(0.0) as u64,
            vram_type: "Unknown".into(),
            vram_bus_width: 0,
            core_clock: 0,
            memory_clock: 0,
            compute_units: 0,
            driver_version: js(&g, "d").unwrap_or_else(|| "Unknown".into()),
            api_version: "Unknown".into(),
            temperature: 0.0,
            power_watts: 0.0,
            fan_speed: 0,
        });
    }

    let os = &hw.os;
    if os.is_object() {
        let build = js(os, "build").unwrap_or_default();
        let ubr = os["ubr"].as_i64();
        let build_full = match (build.is_empty(), ubr) {
            (false, Some(u)) => format!("{}.{}", build, u),
            (false, None) => build.clone(),
            _ => "Unknown".into(),
        };
        let caption = js(os, "caption").unwrap_or_default();
        let name = caption.trim_start_matches("Microsoft ").trim().to_string();
        out.os = Some(OsInfo {
            name: if name.is_empty() { "Windows".into() } else { name },
            version: {
                let d = js(os, "disp").unwrap_or_default();
                let v = js(os, "version").unwrap_or_default();
                match (d.is_empty(), v.is_empty()) {
                    (false, false) => format!("{} ({})", d, v),
                    (false, true) => d,
                    (true, false) => v,
                    _ => "Unknown".into(),
                }
            },
            build: build_full,
            kernel_version: js(os, "version").unwrap_or_else(|| "Unknown".into()),
            is_virtualized: false,
        });
    }

    let b = &hw.bios;
    if b.is_object() {
        let model = or_unknown(js(b, "bp"));
        out.board = Some(MotherboardInfo {
            manufacturer: or_unknown(js(b, "bm")),
            chipset: guess_chipset(&model).map(|c| format!("{} (from board name)", c)).unwrap_or_else(|| "Unknown".into()),
            model,
            version: or_unknown(js(b, "bv")),
            bios_version: or_unknown(js(b, "ver")),
            bios_date: or_unknown(js(b, "date")),
        });
    }
    Some(out)
}

/// Sum of cache sizes in KB from CPUID cache-parameter tuples
/// (level, is_instruction, ways, partitions, line_size, sets). Returns (L1 data, L2, L3) in KB.
pub fn cache_sizes_kb(params: &[(u8, bool, u64, u64, u64, u64)]) -> (u64, u64, u64) {
    let (mut l1, mut l2, mut l3) = (0, 0, 0);
    for &(level, instruction, ways, parts, line, sets) in params {
        let kb = ways * parts * line * sets / 1024;
        match level {
            1 if !instruction => l1 = l1.max(kb),
            2 => l2 = l2.max(kb),
            3 => l3 = l3.max(kb),
            _ => {}
        }
    }
    (l1, l2, l3)
}

// ---------------------------------------------------------------------------------------------
// Platform collectors
// ---------------------------------------------------------------------------------------------

#[cfg(target_os = "windows")]
const WIN_HW_SCRIPT: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
$mem = @(Get-CimInstance Win32_PhysicalMemory | ForEach-Object { @{ cap = [double]$_.Capacity; cfg = [int]$_.ConfiguredClockSpeed; spd = [int]$_.Speed; mfr = $_.Manufacturer; pn = $_.PartNumber; sn = $_.SerialNumber; t = [int]$_.SMBIOSMemoryType } })
$gpu = @(Get-CimInstance Win32_VideoController | ForEach-Object { @{ n = $_.Name; v = $_.AdapterCompatibility; d = $_.DriverVersion; r = [double]$_.AdapterRAM } })
$os = Get-CimInstance Win32_OperatingSystem
$cv = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
$bios = Get-ItemProperty 'HKLM:\HARDWARE\DESCRIPTION\System\BIOS'
[pscustomobject]@{
  mem = $mem; gpu = $gpu
  os = @{ caption = $os.Caption; version = $os.Version; build = $os.BuildNumber; ubr = $cv.UBR; disp = $cv.DisplayVersion }
  bios = @{ bm = $bios.BaseBoardManufacturer; bp = $bios.BaseBoardProduct; bv = $bios.BaseBoardVersion; ver = $bios.BIOSVersion; date = $bios.BIOSReleaseDate }
} | ConvertTo-Json -Compress -Depth 5
"#;

fn hidden(program: &str) -> std::process::Command {
    #[allow(unused_mut)]
    let mut c = std::process::Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    c
}

fn nvidia_gpus() -> Vec<GpuInfo> {
    hidden("nvidia-smi")
        .args([
            "--query-gpu=name,driver_version,memory.total,clocks.max.gr,clocks.max.mem,temperature.gpu,power.draw,fan.speed",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_nvidia_smi_info(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// Merge: nvidia-smi values (accurate VRAM / driver / clocks) replace the generic controller entry
fn merge_gpus(generic: Vec<GpuInfo>, nvidia: Vec<GpuInfo>) -> Vec<GpuInfo> {
    let mut out: Vec<GpuInfo> = generic
        .into_iter()
        .filter(|g| !g.name.to_lowercase().contains("microsoft basic") && !g.name.to_lowercase().contains("remote display"))
        .collect();
    for n in nvidia {
        match out.iter_mut().find(|g| g.name == n.name || (g.vendor.contains("NVIDIA") && n.name.contains(&g.name))) {
            Some(slot) => *slot = n,
            None => out.push(n),
        }
    }
    out
}

#[cfg(target_os = "windows")]
pub fn gather() -> HwExtras {
    let mut hw = hidden("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", WIN_HW_SCRIPT])
        .output()
        .ok()
        .and_then(|o| parse_win_hw(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();
    hw.gpus = merge_gpus(std::mem::take(&mut hw.gpus), nvidia_gpus());
    hw
}

#[cfg(target_os = "linux")]
pub fn gather() -> HwExtras {
    let mut hw = HwExtras::default();
    let (board, _product) = read_dmi(Path::new("/sys/class/dmi/id"));
    hw.board = Some(board);

    let rel = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    hw.os = Some(OsInfo {
        name: parse_os_release(&rel, "PRETTY_NAME").or_else(|| parse_os_release(&rel, "NAME")).unwrap_or_else(|| "Linux".into()),
        version: parse_os_release(&rel, "VERSION_ID").unwrap_or_else(|| "Unknown".into()),
        build: parse_os_release(&rel, "BUILD_ID").unwrap_or_else(|| "rolling/none".into()),
        kernel_version: sysinfo::System::kernel_version().unwrap_or_else(|| "Unknown".into()),
        is_virtualized: false,
    });

    if let Some(out) = hidden("dmidecode").args(["-t", "memory"]).output().ok().filter(|o| o.status.success()) {
        let (mods, t) = parse_dmidecode_memory(&String::from_utf8_lossy(&out.stdout));
        hw.modules = mods;
        hw.memory_type = t;
    }

    let mut generic = Vec::new();
    if let Ok(cards) = std::fs::read_dir("/sys/class/drm") {
        for c in cards.flatten() {
            let name = c.file_name().to_string_lossy().to_string();
            if !(name.starts_with("card") && name[4..].chars().all(|ch| ch.is_ascii_digit())) {
                continue;
            }
            let dev = c.path().join("device");
            let vendor_id = std::fs::read_to_string(dev.join("vendor")).unwrap_or_default().trim().to_string();
            let vendor = match vendor_id.as_str() { "0x10de" => "NVIDIA", "0x1002" => "AMD", "0x8086" => "Intel", _ => continue };
            let vram = std::fs::read_to_string(dev.join("mem_info_vram_total")).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
            generic.push(GpuInfo {
                name: format!("{} GPU ({})", vendor, std::fs::read_to_string(dev.join("device")).unwrap_or_default().trim()),
                vendor: vendor.into(),
                vram_total: vram,
                vram_type: "Unknown".into(),
                vram_bus_width: 0,
                core_clock: 0,
                memory_clock: 0,
                compute_units: 0,
                driver_version: "Unknown".into(),
                api_version: "Unknown".into(),
                temperature: 0.0,
                power_watts: 0.0,
                fan_speed: 0,
            });
        }
    }
    hw.gpus = merge_gpus(generic, nvidia_gpus());
    hw
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn gather() -> HwExtras {
    HwExtras::default()
}

/// CPU cache sizes (L1 data, L2, L3) in KB via CPUID; zeros when unavailable
#[cfg(target_arch = "x86_64")]
pub fn cpu_caches_kb() -> (u64, u64, u64) {
    use raw_cpuid::{CacheType, CpuId};
    let cpuid = CpuId::new();
    let Some(params) = cpuid.get_cache_parameters() else { return (0, 0, 0) };
    let v: Vec<_> = params
        .map(|c| (c.level(), matches!(c.cache_type(), CacheType::Instruction), c.associativity() as u64, c.physical_line_partitions() as u64, c.coherency_line_size() as u64, c.sets() as u64))
        .collect();
    cache_sizes_kb(&v)
}

#[cfg(not(target_arch = "x86_64"))]
pub fn cpu_caches_kb() -> (u64, u64, u64) {
    (0, 0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chipset_is_inferred_from_board_names() {
        assert_eq!(guess_chipset("TUF GAMING Z890-PLUS WIFI").as_deref(), Some("Z890"));
        assert_eq!(guess_chipset("ROG STRIX B650E-F GAMING WIFI").as_deref(), Some("B650E"));
        assert_eq!(guess_chipset("MAG X670E TOMAHAWK").as_deref(), Some("X670E"));
        assert_eq!(guess_chipset("PRIME A320M-K").as_deref(), Some("A320M"));
        assert_eq!(guess_chipset("Some Board 2000"), None);
        assert_eq!(guess_chipset(""), None);
    }

    #[test]
    fn reg_values_and_os_release() {
        let out = "HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\r\n    ProductName    REG_SZ    Windows 10 Pro\r\n    CurrentBuild    REG_SZ    26200\r\n";
        assert_eq!(parse_reg_value(out, "ProductName").as_deref(), Some("Windows 10 Pro"));
        assert_eq!(parse_reg_value(out, "CurrentBuild").as_deref(), Some("26200"));
        assert_eq!(parse_reg_value(out, "Missing"), None);
        let rel = "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 24.04.3 LTS\"\nVERSION_ID=\"24.04\"\n";
        assert_eq!(parse_os_release(rel, "PRETTY_NAME").as_deref(), Some("Ubuntu 24.04.3 LTS"));
        assert_eq!(parse_os_release(rel, "VERSION_ID").as_deref(), Some("24.04"));
        assert_eq!(parse_os_release(rel, "BUILD_ID"), None);
    }

    #[test]
    fn dmi_files_become_board_info() {
        let dir = tempfile::tempdir().unwrap();
        for (f, v) in [("board_vendor", "ASUSTeK COMPUTER INC.\n"), ("board_name", "TUF GAMING Z890-PLUS WIFI\n"), ("board_version", "Rev 1.xx\n"), ("bios_version", "3020\n"), ("bios_date", "04/28/2026\n")] {
            std::fs::write(dir.path().join(f), v).unwrap();
        }
        let (b, _) = read_dmi(dir.path());
        assert_eq!(b.manufacturer, "ASUSTeK COMPUTER INC.");
        assert_eq!(b.model, "TUF GAMING Z890-PLUS WIFI");
        assert_eq!(b.chipset, "Z890 (from board name)");
        assert_eq!(b.bios_version, "3020");
        let (empty, _) = read_dmi(Path::new("/definitely/not/here"));
        assert_eq!(empty.model, "Unknown");
    }

    #[test]
    fn dmidecode_memory_blocks() {
        let text = "Handle 0x0040\nMemory Device\n\tSize: 32 GB\n\tType: DDR5\n\tSpeed: 6400 MT/s\n\tManufacturer: G.Skill\n\tPart Number: F5-6400J3239G32G\n\tSerial Number: 00000000\n\tConfigured Memory Speed: 6000 MT/s\n\tConfigured Voltage: 1.35 V\n\nHandle 0x0041\nMemory Device\n\tSize: No Module Installed\n\tType: Unknown\n\nHandle 0x0042\nMemory Device\n\tSize: 32 GB\n\tType: DDR5\n\tSpeed: 6400 MT/s\n\tManufacturer: G.Skill\n\tPart Number: F5-6400J3239G32G\n";
        let (mods, t) = parse_dmidecode_memory(text);
        assert_eq!(mods.len(), 2);
        assert_eq!(mods[0].size, 32 << 30);
        assert_eq!(mods[0].speed, 6000); // configured beats rated
        assert_eq!(mods[1].speed, 6400);
        assert_eq!(mods[0].manufacturer, "G.Skill");
        assert!((mods[0].timings.voltage - 1.35).abs() < 1e-6);
        assert_eq!(t.as_deref(), Some("DDR5"));
    }

    #[test]
    fn nvidia_smi_info_line() {
        let g = parse_nvidia_smi_info("NVIDIA GeForce RTX 5090, 591.74, 32607, 3090, 14001, 37, 27.5, 30\n");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].name, "NVIDIA GeForce RTX 5090");
        assert_eq!(g[0].vram_total, 32607u64 * 1048576);
        assert_eq!(g[0].core_clock, 3090);
        assert_eq!(g[0].driver_version, "591.74");
        assert!(parse_nvidia_smi_info("garbage").is_empty());
    }

    #[test]
    fn windows_hardware_json() {
        let json = r#"{"mem":[{"cap":34359738368,"cfg":6000,"spd":6400,"mfr":"G.Skill","pn":"F5-6400J3239G32G","sn":"1234","t":34},{"cap":34359738368,"cfg":6000,"spd":6400,"mfr":"G.Skill","pn":"F5-6400J3239G32G","sn":"5678","t":34}],
          "gpu":{"n":"NVIDIA GeForce RTX 5090","v":"NVIDIA","d":"32.0.15.9174","r":4293918720},
          "os":{"caption":"Microsoft Windows 11 Pro","version":"10.0.26200","build":"26200","ubr":9550,"disp":"25H2"},
          "bios":{"bm":"ASUSTeK COMPUTER INC.","bp":"TUF GAMING Z890-PLUS WIFI","bv":"Rev 1.xx","ver":"3020","date":"04/28/2026"}}"#;
        let hw = parse_win_hw(json).unwrap();
        assert_eq!(hw.modules.len(), 2);
        assert_eq!(hw.modules[0].speed, 6000);
        assert_eq!(hw.memory_type.as_deref(), Some("DDR5"));
        assert_eq!(hw.gpus.len(), 1); // PowerShell collapses one-element arrays to an object
        assert_eq!(hw.gpus[0].driver_version, "32.0.15.9174");
        let os = hw.os.unwrap();
        assert_eq!(os.name, "Windows 11 Pro");
        assert_eq!(os.build, "26200.9550");
        assert_eq!(os.version, "25H2 (10.0.26200)");
        let b = hw.board.unwrap();
        assert_eq!(b.model, "TUF GAMING Z890-PLUS WIFI");
        assert_eq!(b.chipset, "Z890 (from board name)");
        assert!(parse_win_hw("nope").is_none());
    }

    #[test]
    fn nvidia_smi_replaces_the_generic_controller_entry() {
        let generic = parse_win_hw(r#"{"gpu":[{"n":"NVIDIA GeForce RTX 5090","v":"NVIDIA","d":"1.0","r":4293918720},{"n":"Microsoft Basic Display Adapter","v":"Microsoft","d":"1","r":0}]}"#).unwrap().gpus;
        let nv = parse_nvidia_smi_info("NVIDIA GeForce RTX 5090, 591.74, 32607, 3090, 14001, 37, 27.5, 30");
        let merged = merge_gpus(generic, nv);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].vram_total, 32607u64 * 1048576); // not the 4 GB-wrapped WMI figure
        assert_eq!(merged[0].driver_version, "591.74");
    }

    #[test]
    fn cache_sizes_from_cpuid_parameters() {
        // (level, instruction, ways, partitions, line, sets)
        let p = [(1, false, 12, 1, 64, 64), (1, true, 8, 1, 64, 64), (2, false, 10, 1, 64, 4096), (3, false, 12, 1, 64, 49152)];
        assert_eq!(cache_sizes_kb(&p), (48, 2560, 36864));
    }

    #[test]
    fn this_machine_reports_some_cache() {
        #[cfg(target_arch = "x86_64")]
        {
            let (l1, l2, _l3) = cpu_caches_kb();
            // VMs may hide them, but a real CPUID answer is never absurd
            assert!(l1 < 1024 && l2 < 65536);
        }
    }
}
