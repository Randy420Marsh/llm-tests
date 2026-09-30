//! System information collection for CPU, GPU, memory, temperatures, and frequencies

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sysinfo::System;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    /// The primary (largest-VRAM) GPU
    pub gpu: Option<GpuInfo>,
    /// Every GPU found (iGPU + dGPU)
    #[serde(default)]
    pub gpus: Vec<GpuInfo>,
    pub motherboard: MotherboardInfo,
    pub os: OsInfo,
    pub virtualization: VirtualizationInfo,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuInfo {
    pub name: String,
    pub vendor: String,
    pub brand: String,
    pub frequency: u64,        // Base frequency in MHz
    pub max_frequency: u64,    // Max turbo frequency in MHz
    pub cores: usize,          // Physical cores
    pub threads: usize,        // Logical threads
    pub p_cores: usize,        // Performance cores (Intel hybrid)
    pub e_cores: usize,        // Efficiency cores (Intel hybrid)
    pub l1_cache: u64,         // KB
    pub l2_cache: u64,         // KB
    pub l3_cache: u64,         // KB
    pub architecture: String,
    pub microarchitecture: String,
    pub features: Vec<String>, // AVX, AVX2, AVX-512, SSE, etc.
    pub current_frequencies: Vec<u64>, // Per-core current frequency in MHz
    pub temperatures: Vec<f32>, // Per-core temperatures in Celsius
    pub power_watts: Option<f32>, // Package power in watts
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryInfo {
    pub total: u64,            // Bytes
    pub available: u64,        // Bytes
    pub used: u64,             // Bytes
    pub speed: u32,            // MT/s
    pub type_: String,         // DDR4, DDR5, etc.
    pub channels: u32,
    pub timings: MemoryTimings,
    pub modules: Vec<MemoryModule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryTimings {
    pub cl: u32,               // CAS Latency
    pub trcd: u32,             // RAS to CAS Delay
    pub trp: u32,              // RAS Precharge
    pub tras: u32,             // Active to Precharge Delay
    pub trc: u32,              // Active to Active Delay
    pub voltage: f32,          // Voltage in V
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryModule {
    pub size: u64,             // Bytes
    pub speed: u32,            // MT/s
    pub manufacturer: String,
    pub part_number: String,
    pub serial_number: String,
    pub timings: MemoryTimings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    pub vendor: String,        // NVIDIA, AMD, Intel
    pub vram_total: u64,       // Bytes
    pub vram_type: String,     // GDDR6, GDDR6X, HBM2, etc.
    pub vram_bus_width: u32,   // bits
    pub core_clock: u32,       // MHz
    pub memory_clock: u32,     // MHz
    pub compute_units: u32,    // SMs, CUs, XEs
    pub driver_version: String,
    pub api_version: String,   // Vulkan, DirectX, CUDA versions
    pub temperature: f32,      // Celsius
    pub power_watts: f32,
    pub fan_speed: u32,        // RPM or percentage
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MotherboardInfo {
    pub manufacturer: String,
    pub model: String,
    pub version: String,
    pub bios_version: String,
    pub bios_date: String,
    pub chipset: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OsInfo {
    pub name: String,
    pub version: String,
    pub build: String,
    pub kernel_version: String,
    pub is_virtualized: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualizationInfo {
    pub bios_virtualization_enabled: bool,    // VT-x / SVM in BIOS
    pub hyper_v_enabled: bool,                // Windows Hyper-V
    pub vbs_enabled: bool,                    // Virtualization-Based Security
    pub hvci_enabled: bool,                   // Hypervisor-Protected Code Integrity (Memory Integrity)
    pub wsl_enabled: bool,                    // Windows Subsystem for Linux
    pub kvm_enabled: bool,                    // Linux KVM
    pub vmware_detected: bool,
    pub virtualbox_detected: bool,
    /// Running as a KVM guest (hypervisor signature "KVMKVMKVM")
    #[serde(default)]
    pub kvm_guest: bool,
    pub details: String,
}

pub struct SystemInfoCollector {
    sys: System,
}

impl SystemInfoCollector {
    pub fn new() -> Self {
        let mut sys = System::new_all();
        sys.refresh_all();
        Self { sys }
    }

    pub fn collect(&mut self) -> Result<SystemInfo> {
        self.sys.refresh_all();

        let extras = crate::hwinfo::gather();
        let cpu = self.collect_cpu_info()?;
        let mut memory = self.collect_memory_info()?;
        if !extras.modules.is_empty() {
            memory.speed = extras.modules.iter().map(|m| m.speed).max().unwrap_or(0);
            // Consumer platforms run at most two channels regardless of how many DIMMs are fitted
            memory.channels = (extras.modules.len() as u32).min(2);
            memory.modules = extras.modules.clone();
        }
        if let Some(t) = &extras.memory_type {
            memory.type_ = t.clone();
        }
        let gpus = extras.gpus.clone();
        let gpu = gpus.iter().max_by_key(|g| g.vram_total).cloned();
        let motherboard = extras.board.clone().unwrap_or_else(|| self.collect_motherboard_info());
        let mut os = extras.os.clone().unwrap_or_else(|| self.collect_os_info());
        let virtualization = self.collect_virtualization_info()?;
        os.is_virtualized = virtualization.vmware_detected || virtualization.virtualbox_detected || virtualization.kvm_guest;

        Ok(SystemInfo {
            cpu,
            memory,
            gpu,
            gpus,
            motherboard,
            os,
            virtualization,
            timestamp: chrono::Utc::now().to_rfc3339(),
        })
    }

    fn collect_cpu_info(&mut self) -> Result<CpuInfo> {
        self.sys.refresh_cpu();
        
        let cpus = self.sys.cpus();
        let cpu = &cpus[0]; // First CPU for general info
        
        let mut current_frequencies = Vec::new();
        let temperatures = Vec::new();
        
        for cpu in cpus {
            current_frequencies.push(cpu.frequency());
            // Temperature would need platform-specific code
        }

        // Get CPU brand and vendor from raw-cpuid
        let (vendor, brand, features) = self.get_cpuid_info();
        
        let caches = crate::hwinfo::cpu_caches_kb();

        // Detect P-cores and E-cores (Intel hybrid architecture)
        let (p_cores, e_cores) = self.detect_hybrid_cores();

        Ok(CpuInfo {
            // sysinfo reports "CPU 1" on Windows; the CPUID brand string is the real model name
            name: if brand.trim().is_empty() || brand == "Unknown" {
                cpu.name().to_string()
            } else {
                brand.trim().to_string()
            },
            vendor: vendor.clone(),
            brand: brand.clone(),
            frequency: cpu.frequency(),
            max_frequency: cpu.frequency(), // sysinfo 0.30 doesn't have max_frequency
            cores: self.sys.physical_core_count().unwrap_or(cpus.len()),
            threads: cpus.len(),
            p_cores,
            e_cores,
            l1_cache: caches.0,
            l2_cache: caches.1,
            l3_cache: caches.2,
            architecture: std::env::consts::ARCH.to_string(),
            microarchitecture: self.detect_microarchitecture(&vendor, &brand),
            features,
            current_frequencies,
            temperatures,
            power_watts: None,
        })
    }

    fn get_cpuid_info(&self) -> (String, String, Vec<String>) {
        #[cfg(target_arch = "x86_64")]
        {
            use raw_cpuid::CpuId;
            let cpuid = CpuId::new();
            
            let vendor = cpuid.get_vendor_info()
                .map(|v| v.as_str().to_string())
                .unwrap_or_else(|| "Unknown".to_string());
            
            let brand = cpuid.get_processor_brand_string()
                .map(|b| b.as_str().to_string())
                .unwrap_or_else(|| "Unknown".to_string());
            
            let mut features = Vec::new();
            if let Some(feature_info) = cpuid.get_feature_info() {
                if feature_info.has_sse() { features.push("SSE".to_string()); }
                if feature_info.has_sse2() { features.push("SSE2".to_string()); }
                if feature_info.has_sse3() { features.push("SSE3".to_string()); }
                if feature_info.has_ssse3() { features.push("SSSE3".to_string()); }
                if feature_info.has_sse41() { features.push("SSE4.1".to_string()); }
                if feature_info.has_sse42() { features.push("SSE4.2".to_string()); }
                if feature_info.has_avx() { features.push("AVX".to_string()); }
                if feature_info.has_fma() { features.push("FMA".to_string()); }
                if feature_info.has_aesni() { features.push("AES-NI".to_string()); }
                // SHA detection is more complex in raw-cpuid, skipping for now or using a generic check
                if feature_info.has_rdrand() { features.push("RDRAND".to_string()); }
                // RDSEED not directly available in FeatureInfo, skipping or using a generic check
            }
            
            // Check for AVX-512
            if let Some(ext_features) = cpuid.get_extended_feature_info() {
                if ext_features.has_avx2() { features.push("AVX2".to_string()); }
                if ext_features.has_avx512f() { features.push("AVX-512F".to_string()); }
                if ext_features.has_avx512dq() { features.push("AVX-512DQ".to_string()); }
                if ext_features.has_avx512cd() { features.push("AVX-512CD".to_string()); }
                if ext_features.has_avx512bw() { features.push("AVX-512BW".to_string()); }
                if ext_features.has_avx512vl() { features.push("AVX-512VL".to_string()); }
                if ext_features.has_avx512_ifma() { features.push("AVX-512IFMA".to_string()); }
                if ext_features.has_avx512vbmi() { features.push("AVX-512VBMI".to_string()); }
            }
            
            (vendor, brand, features)
        }
        
        #[cfg(not(target_arch = "x86_64"))]
        {
            ("Unknown".to_string(), "Unknown".to_string(), Vec::new())
        }
    }

    fn detect_microarchitecture(&self, vendor: &str, brand: &str) -> String {
        // Simplified microarchitecture detection based on brand string
        let brand_lower = brand.to_lowercase();
        
        if vendor.contains("GenuineIntel") {
            if brand_lower.contains("13th") || brand_lower.contains("14th") || brand_lower.contains("core ultra") {
                "Raptor Lake / Meteor Lake".to_string()
            } else if brand_lower.contains("12th") {
                "Alder Lake".to_string()
            } else if brand_lower.contains("11th") {
                "Rocket Lake / Tiger Lake".to_string()
            } else if brand_lower.contains("10th") {
                "Comet Lake / Ice Lake".to_string()
            } else if brand_lower.contains("9th") {
                "Coffee Lake Refresh".to_string()
            } else if brand_lower.contains("8th") {
                "Coffee Lake".to_string()
            } else if brand_lower.contains("7th") {
                "Kaby Lake".to_string()
            } else if brand_lower.contains("6th") {
                "Skylake".to_string()
            } else {
                "Intel (Unknown Gen)".to_string()
            }
        } else if vendor.contains("AuthenticAMD") {
            if brand_lower.contains("ryzen 9000") || brand_lower.contains("ryzen ai 300") {
                "Zen 5".to_string()
            } else if brand_lower.contains("ryzen 7000") {
                "Zen 4".to_string()
            } else if brand_lower.contains("ryzen 5000") {
                "Zen 3".to_string()
            } else if brand_lower.contains("ryzen 4000") || brand_lower.contains("ryzen 3000") {
                "Zen 2".to_string()
            } else if brand_lower.contains("ryzen 2000") || brand_lower.contains("ryzen 1000") {
                "Zen / Zen+".to_string()
            } else if brand_lower.contains("epyc") {
                "Zen (Server)".to_string()
            } else {
                "AMD (Unknown Gen)".to_string()
            }
        } else {
            "Unknown".to_string()
        }
    }

    fn detect_hybrid_cores(&self) -> (usize, usize) {
        // Intel hybrid architecture: logical P/E thread counts
        // (e.g. Core Ultra 270K = 8 P-threads + 12 E-threads, no SMT)
        let (p, e) = crate::topology::hybrid_counts();
        // Only hybrid CPUs have a P/E split; otherwise report (0, 0) rather than "all cores are P"
        if e == 0 { (0, 0) } else { (p, e) }
    }

    fn collect_memory_info(&mut self) -> Result<MemoryInfo> {
        self.sys.refresh_memory();
        
        let total = self.sys.total_memory();
        let used = self.sys.used_memory();
        let available = total.saturating_sub(used);
        
        // Memory speed and timings would need platform-specific code (WMI on Windows, dmidecode on Linux)
        Ok(MemoryInfo {
            total,
            available,
            used,
            speed: 0, // Would need platform-specific detection
            type_: "Unknown".to_string(),
            channels: 0,
            timings: MemoryTimings {
                cl: 0,
                trcd: 0,
                trp: 0,
                tras: 0,
                trc: 0,
                voltage: 0.0,
            },
            modules: Vec::new(),
        })
    }

    fn collect_motherboard_info(&self) -> MotherboardInfo {
        // Would need platform-specific code (WMI on Windows, dmidecode on Linux)
        MotherboardInfo {
            manufacturer: "Unknown".to_string(),
            model: "Unknown".to_string(),
            version: "Unknown".to_string(),
            bios_version: "Unknown".to_string(),
            bios_date: "Unknown".to_string(),
            chipset: "Unknown".to_string(),
        }
    }

    fn collect_os_info(&self) -> OsInfo {
        OsInfo {
            name: System::name().unwrap_or_else(|| std::env::consts::OS.to_string()),
            version: System::os_version().unwrap_or_else(|| "Unknown".to_string()),
            build: System::long_os_version().unwrap_or_else(|| "Unknown".to_string()),
            kernel_version: System::kernel_version().unwrap_or_else(|| "Unknown".to_string()),
            is_virtualized: false,
        }
    }

    /// Built from the unprivileged detector (see virtualization.rs) so the saved results agree with the GUI
    fn collect_virtualization_info(&self) -> Result<VirtualizationInfo> {
        let status = crate::virtualization::VirtualizationDetector::detect()?;
        let vendor = crate::virtualization::hypervisor_vendor().unwrap_or_default();
        let win = status.windows.as_ref();
        let lnx = status.linux.as_ref();
        Ok(VirtualizationInfo {
            bios_virtualization_enabled: status.bios.vt_x_enabled || status.bios.svm_enabled,
            hyper_v_enabled: win.map_or(false, |w| w.hyper_v_enabled),
            vbs_enabled: win.map_or(false, |w| w.vbs_enabled),
            hvci_enabled: win.map_or(false, |w| w.hvci_enabled),
            wsl_enabled: win.map_or(false, |w| w.wsl_enabled),
            kvm_enabled: lnx.map_or(false, |l| l.kvm_enabled),
            kvm_guest: vendor.starts_with("KVM"),
            vmware_detected: vendor.starts_with("VMware"),
            virtualbox_detected: vendor.starts_with("VBox"),
            details: format!("hypervisor vendor: {}; {}", if vendor.is_empty() { "none" } else { &vendor }, status.bios.details),
        })
    }
}

impl Default for SystemInfoCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Collect system information once (for logging)
static CACHE: std::sync::Mutex<Option<(std::time::Instant, SystemInfo)>> = std::sync::Mutex::new(None);

/// System information, reused for two minutes: gathering it can spawn PowerShell, and every
/// benchmark run asks for it.
pub fn collect_system_info() -> Result<SystemInfo> {
    if let Some((at, info)) = CACHE.lock().unwrap().as_ref() {
        if at.elapsed() < std::time::Duration::from_secs(120) {
            return Ok(info.clone());
        }
    }
    collect_system_info_fresh()
}

/// Always re-reads everything (the Refresh button)
pub fn collect_system_info_fresh() -> Result<SystemInfo> {
    let mut collector = SystemInfoCollector::new();
    let info = collector.collect()?;
    *CACHE.lock().unwrap() = Some((std::time::Instant::now(), info.clone()));
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collector_creation() {
        let _collector = SystemInfoCollector::new();
        // Just verify it creates without panic
        assert!(true);
    }
}