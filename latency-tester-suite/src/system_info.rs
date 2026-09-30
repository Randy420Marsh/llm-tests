//! System information collection for CPU, GPU, memory, temperatures, and frequencies

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sysinfo::System;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    pub gpu: Option<GpuInfo>,
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
        
        let cpu = self.collect_cpu_info()?;
        let memory = self.collect_memory_info()?;
        let gpu = self.collect_gpu_info();
        let motherboard = self.collect_motherboard_info();
        let os = self.collect_os_info();
        let virtualization = self.collect_virtualization_info()?;

        Ok(SystemInfo {
            cpu,
            memory,
            gpu,
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
            l1_cache: 0, // Would need platform-specific detection
            l2_cache: 0,
            l3_cache: 0,
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
        if e == 0 { (p, 0) } else { (p, e) }
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

    fn collect_gpu_info(&self) -> Option<GpuInfo> {
        // GPU info would need platform-specific code:
        // - Windows: WMI, DXGI, NVAPI, ADL
        // - Linux: DRM, sysfs, nvidia-smi, rocm-smi
        // For now, return None - would be implemented with platform-specific backends
        None
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
            name: std::env::consts::OS.to_string(),
            version: "Unknown".to_string(),
            build: "Unknown".to_string(),
            kernel_version: "Unknown".to_string(),
            is_virtualized: false, // Would be detected in virtualization_info
        }
    }

    fn collect_virtualization_info(&self) -> Result<VirtualizationInfo> {
        #[cfg(target_os = "windows")]
        {
            self.collect_windows_virtualization_info()
        }
        
        #[cfg(target_os = "linux")]
        {
            self.collect_linux_virtualization_info()
        }
        
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            Ok(VirtualizationInfo {
                bios_virtualization_enabled: false,
                hyper_v_enabled: false,
                vbs_enabled: false,
                hvci_enabled: false,
                wsl_enabled: false,
                kvm_enabled: false,
                vmware_detected: false,
                virtualbox_detected: false,
                details: "Unsupported platform".to_string(),
            })
        }
    }

    #[cfg(target_os = "windows")]
    fn collect_windows_virtualization_info(&self) -> Result<VirtualizationInfo> {
        let mut details = String::new();
        
        // Check if running in a VM
        let mut vmware_detected = false;
        let mut virtualbox_detected = false;
        
        // Check for VMware
        if let Ok(output) = std::process::Command::new("wmic")
            .args(["computersystem", "get", "manufacturer"])
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            if output_str.to_lowercase().contains("vmware") {
                vmware_detected = true;
                details.push_str("VMware detected; ");
            }
            if output_str.to_lowercase().contains("virtualbox") || output_str.to_lowercase().contains("innotek") {
                virtualbox_detected = true;
                details.push_str("VirtualBox detected; ");
            }
        }
        
        // Check Hyper-V
        let mut hyper_v_enabled = false;
        if let Ok(output) = std::process::Command::new("powershell")
            .args(["-Command", "Get-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V-All | Select-Object State"])
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            if output_str.contains("Enabled") {
                hyper_v_enabled = true;
                details.push_str("Hyper-V enabled; ");
            }
        }
        
        // Check VBS/HVCI (Memory Integrity)
        let mut vbs_enabled = false;
        let mut hvci_enabled = false;
        if let Ok(output) = std::process::Command::new("powershell")
            .args(["-Command", "Get-CimInstance -Namespace root\\Microsoft\\Windows\\DeviceGuard -ClassName DeviceGuardSecurityProperties | Select-Object VirtualizationBasedSecurityStatus, RequiredSecurityProperties"])
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            if output_str.contains("2") { // VBS running
                vbs_enabled = true;
                details.push_str("VBS enabled; ");
            }
            if output_str.contains("1") { // HVCI enabled
                hvci_enabled = true;
                details.push_str("HVCI (Memory Integrity) enabled; ");
            }
        }
        
        // Check WSL
        let mut wsl_enabled = false;
        if let Ok(output) = std::process::Command::new("wsl")
            .args(["--status"])
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            if output_str.contains("WSL 2") || output_str.contains("WSL 1") {
                wsl_enabled = true;
                details.push_str("WSL enabled; ");
            }
        }
        
        // BIOS virtualization (VT-x/SVM) - would need WMI or registry check
        let bios_virtualization_enabled = true; // Assume enabled if we can run VMs
        
        Ok(VirtualizationInfo {
            bios_virtualization_enabled,
            hyper_v_enabled,
            vbs_enabled,
            hvci_enabled,
            wsl_enabled,
            kvm_enabled: false,
            vmware_detected,
            virtualbox_detected,
            details,
        })
    }

    #[cfg(target_os = "linux")]
    fn collect_linux_virtualization_info(&self) -> Result<VirtualizationInfo> {
        let mut details = String::new();
        let mut bios_virtualization_enabled = false;
        let mut kvm_enabled = false;
        let mut vmware_detected = false;
        let mut virtualbox_detected = false;
        
        // Check CPU flags for virtualization support
        if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
            if cpuinfo.contains("vmx") || cpuinfo.contains("svm") {
                bios_virtualization_enabled = true;
                details.push_str("CPU virtualization extensions (VMX/SVM) present; ");
            }
        }
        
        // Check KVM
        if std::path::Path::new("/dev/kvm").exists() {
            kvm_enabled = true;
            details.push_str("KVM available; ");
        }
        
        // Check for VMware
        if let Ok(output) = std::process::Command::new("systemd-detect-virt")
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            if output_str.contains("vmware") {
                vmware_detected = true;
                details.push_str("VMware detected; ");
            } else if output_str.contains("virtualbox") || output_str.contains("vbox") {
                virtualbox_detected = true;
                details.push_str("VirtualBox detected; ");
            } else if output_str.trim() != "none" {
                details.push_str(&format!("Running in VM: {}; ", output_str.trim()));
            }
        }
        
        // Check for disabled virtualization in kernel cmdline
        if let Ok(cmdline) = std::fs::read_to_string("/proc/cmdline") {
            if cmdline.contains("kvm_intel.enable_virt_at_load=0") || 
               cmdline.contains("kvm_amd.enable_virt_at_load=0") ||
               cmdline.contains("nokvm") {
                details.push_str("KVM disabled via kernel cmdline; ");
            }
        }
        
        Ok(VirtualizationInfo {
            bios_virtualization_enabled,
            hyper_v_enabled: false,
            vbs_enabled: false,
            hvci_enabled: false,
            wsl_enabled: false,
            kvm_enabled,
            vmware_detected,
            virtualbox_detected,
            details,
        })
    }
}

impl Default for SystemInfoCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Collect system information once (for logging)
pub fn collect_system_info() -> Result<SystemInfo> {
    let mut collector = SystemInfoCollector::new();
    collector.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collector_creation() {
        let collector = SystemInfoCollector::new();
        // Just verify it creates without panic
        assert!(true);
    }
}