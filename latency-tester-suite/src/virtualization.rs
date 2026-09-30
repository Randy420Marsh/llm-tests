//! Virtualization detection and settings information for Windows and Linux

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualizationStatus {
    pub platform: Platform,
    pub bios: BiosVirtualization,
    pub windows: Option<WindowsVirtualization>,
    pub linux: Option<LinuxVirtualization>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Platform {
    Windows,
    Linux,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BiosVirtualization {
    pub vt_x_enabled: bool,           // Intel VT-x
    pub svm_enabled: bool,            // AMD SVM
    pub vt_d_enabled: bool,           // Intel VT-d
    pub iommu_enabled: bool,          // IOMMU
    pub tpm_enabled: bool,            // TPM
    pub secure_boot_enabled: bool,    // Secure Boot
    pub details: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowsVirtualization {
    pub hyper_v_enabled: bool,
    pub vbs_enabled: bool,            // Virtualization-Based Security
    pub hvci_enabled: bool,           // Hypervisor-Protected Code Integrity (Memory Integrity)
    pub wsl_enabled: bool,
    pub wsl_version: Option<String>,
    pub core_isolation_enabled: bool,
    pub memory_integrity_enabled: bool,
    pub virtual_machine_platform_enabled: bool,
    pub windows_hypervisor_platform_enabled: bool,
    pub details: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinuxVirtualization {
    pub kvm_enabled: bool,
    pub kvm_intel_loaded: bool,
    pub kvm_amd_loaded: bool,
    pub vmx_svm_in_cpuinfo: bool,
    pub nested_virt_enabled: bool,
    pub kernel_cmdline_virt_disabled: bool,
    pub systemd_detect_virt: String,
    pub details: String,
}

/// Everything Windows can tell an unprivileged process about virtualization.
/// Kept platform independent so the PowerShell-output parser can be unit tested anywhere.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub struct WinFacts {
    /// Firmware virtualization (VT-x/SVM) enabled, as Task Manager reports it
    pub firmware_virt_enabled: bool,
    /// A hypervisor is running underneath the OS (Hyper-V, VBS, WSL2, ...)
    pub hypervisor_present: bool,
    /// Win32_DeviceGuard.VirtualizationBasedSecurityStatus: 0 off, 1 configured, 2 running
    pub vbs_status: i64,
    /// SecurityServicesRunning: 1 = Credential Guard, 2 = HVCI (memory integrity)
    pub services_running: Vec<i64>,
    /// AvailableSecurityProperties: 1 hypervisor, 2 secure boot, 3 DMA protection, ...
    pub available_props: Vec<i64>,
    pub feature_hyper_v: bool,
    pub feature_vm_platform: bool,
    pub feature_hypervisor_platform: bool,
    pub feature_wsl: bool,
    pub secure_boot: bool,
    pub tpm_present: bool,
    pub raw: String,
}

/// PowerShell run without elevation: CIM classes and registry values readable by any user
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const WIN_FACTS_SCRIPT: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
$dg = Get-CimInstance -Namespace root\Microsoft\Windows\DeviceGuard -ClassName Win32_DeviceGuard
$cs = Get-CimInstance -ClassName Win32_ComputerSystem
$names = 'Microsoft-Hyper-V-All','VirtualMachinePlatform','HypervisorPlatform','Microsoft-Windows-Subsystem-Linux'
$feat = @(Get-CimInstance -ClassName Win32_OptionalFeature | Where-Object { $names -contains $_.Name } | ForEach-Object { @{ n = $_.Name; s = [int]$_.InstallState } })
$sb = (Get-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\SecureBoot\State').UEFISecureBootEnabled
# Win32_Tpm only answers to administrators, so a normal run always saw "no TPM". The PnP entry for the
# TPM ("Trusted Platform Module 2.0", class SecurityDevices) is readable by any user.
$tpm = Get-CimInstance -Namespace root\cimv2\Security\MicrosoftTpm -ClassName Win32_Tpm
$tpmdev = @(Get-CimInstance -ClassName Win32_PnPEntity -Filter "PNPClass='SecurityDevices'" | Where-Object { $_.Name -match 'Trusted Platform Module|TPM' -and $_.ConfigManagerErrorCode -eq 0 })
[pscustomobject]@{
  vbs = [int]$dg.VirtualizationBasedSecurityStatus
  running = @($dg.SecurityServicesRunning)
  avail = @($dg.AvailableSecurityProperties)
  hv = [bool]$cs.HypervisorPresent
  feat = $feat
  secureboot = [int]$sb
  tpm = [bool]$tpm
  tpmdev = ($tpmdev.Count -gt 0)
} | ConvertTo-Json -Compress -Depth 4
"#;

/// Parse the JSON printed by `WIN_FACTS_SCRIPT` (tolerates PowerShell's scalar/array quirks)
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn parse_win_facts(json: &str) -> Option<WinFacts> {
    use serde_json::Value;
    // PowerShell may print warnings before the JSON object
    let start = json.find('{')?;
    let v: Value = serde_json::from_str(json[start..].trim()).ok()?;
    let ints = |v: &Value| -> Vec<i64> {
        match v {
            Value::Array(a) => a.iter().filter_map(Value::as_i64).collect(),
            Value::Number(n) => n.as_i64().into_iter().collect(),
            _ => Vec::new(),
        }
    };
    let feature = |name: &str| -> bool {
        let list: Vec<&Value> = match &v["feat"] {
            Value::Array(a) => a.iter().collect(),
            o @ Value::Object(_) => vec![o],
            _ => Vec::new(),
        };
        // InstallState 1 = installed/enabled
        list.iter().any(|f| f["n"].as_str() == Some(name) && f["s"].as_i64() == Some(1))
    };
    Some(WinFacts {
        firmware_virt_enabled: false, // filled in from native APIs
        hypervisor_present: v["hv"].as_bool().unwrap_or(false),
        vbs_status: v["vbs"].as_i64().unwrap_or(0),
        services_running: ints(&v["running"]),
        available_props: ints(&v["avail"]),
        feature_hyper_v: feature("Microsoft-Hyper-V-All"),
        feature_vm_platform: feature("VirtualMachinePlatform"),
        feature_hypervisor_platform: feature("HypervisorPlatform"),
        feature_wsl: feature("Microsoft-Windows-Subsystem-Linux"),
        secure_boot: v["secureboot"].as_i64() == Some(1),
        tpm_present: v["tpm"].as_bool().unwrap_or(false) || v["tpmdev"].as_bool().unwrap_or(false),
        raw: json.trim().to_string(),
    })
}

/// CPUID: is a hypervisor running underneath us? (leaf 1 ECX bit 31)
#[cfg(target_arch = "x86_64")]
fn cpuid_hypervisor_present() -> bool {
    core::arch::x86_64::__cpuid(1).ecx & (1 << 31) != 0
}

#[cfg(not(target_arch = "x86_64"))]
#[allow(dead_code)]
fn cpuid_hypervisor_present() -> bool {
    false
}

/// Hypervisor signature from CPUID leaf 0x40000000 ("VMwareVMware", "KVMKVMKVM", "Microsoft Hv", ...)
#[cfg(target_arch = "x86_64")]
pub fn hypervisor_vendor() -> Option<String> {
    if !cpuid_hypervisor_present() {
        return None;
    }
    let r = core::arch::x86_64::__cpuid(0x4000_0000);
    let mut b = Vec::with_capacity(12);
    for reg in [r.ebx, r.ecx, r.edx] {
        b.extend_from_slice(&reg.to_le_bytes());
    }
    let s = String::from_utf8_lossy(&b).trim_end_matches('\0').to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(not(target_arch = "x86_64"))]
pub fn hypervisor_vendor() -> Option<String> {
    None
}

/// CPU vendor string from CPUID leaf 0
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
fn cpuid_vendor() -> String {
    let r = core::arch::x86_64::__cpuid(0);
    let mut b = Vec::with_capacity(12);
    for reg in [r.ebx, r.edx, r.ecx] {
        b.extend_from_slice(&reg.to_le_bytes());
    }
    String::from_utf8_lossy(&b).to_string()
}

#[cfg(not(target_arch = "x86_64"))]
#[allow(dead_code)]
fn cpuid_vendor() -> String {
    String::new()
}

#[cfg(target_os = "windows")]
fn gather_win_facts() -> WinFacts {
    use std::os::windows::process::CommandExt;
    use windows::Win32::System::Threading::{IsProcessorFeaturePresent, PROCESSOR_FEATURE_ID};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000; // no console flash from the GUI exe
    let mut facts = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", WIN_FACTS_SCRIPT])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()
        .and_then(|o| parse_win_facts(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();

    // PF_VIRT_FIRMWARE_ENABLED (21): the flag behind Task Manager's "Virtualisation: Enabled"
    facts.firmware_virt_enabled =
        unsafe { IsProcessorFeaturePresent(PROCESSOR_FEATURE_ID(21)) }.as_bool();
    facts.hypervisor_present |= cpuid_hypervisor_present();
    facts
}

pub struct VirtualizationDetector;

impl VirtualizationDetector {
    pub fn detect() -> Result<VirtualizationStatus> {
        let platform = Self::detect_platform();

        #[cfg(target_os = "windows")]
        let facts = gather_win_facts();

        #[cfg(target_os = "windows")]
        let bios = Self::detect_bios_virtualization_windows(&facts)?;
        #[cfg(not(target_os = "windows"))]
        let bios = Self::detect_bios_virtualization()?;

        let windows = if platform == Platform::Windows {
            #[cfg(target_os = "windows")]
            { Some(Self::windows_from_facts(&facts)) }
            #[cfg(not(target_os = "windows"))]
            { None }
        } else {
            None
        };
        let linux = if platform == Platform::Linux {
            Some(Self::detect_linux_virtualization()?)
        } else {
            None
        };
        
        let recommendations = Self::generate_recommendations(&bios, &windows, &linux);

        Ok(VirtualizationStatus {
            platform,
            bios,
            windows,
            linux,
            recommendations,
        })
    }

    fn detect_platform() -> Platform {
        if cfg!(target_os = "windows") {
            Platform::Windows
        } else if cfg!(target_os = "linux") {
            Platform::Linux
        } else {
            Platform::Unknown
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn detect_bios_virtualization() -> Result<BiosVirtualization> {
        #[cfg(target_os = "linux")]
        {
            Self::detect_bios_virtualization_linux()
        }

        #[cfg(not(target_os = "linux"))]
        {
            Ok(BiosVirtualization {
                vt_x_enabled: false,
                svm_enabled: false,
                vt_d_enabled: false,
                iommu_enabled: false,
                tpm_enabled: false,
                secure_boot_enabled: false,
                details: "Unsupported platform".to_string(),
            })
        }
    }

    #[cfg(target_os = "windows")]
    fn detect_bios_virtualization_windows(f: &WinFacts) -> Result<BiosVirtualization> {
        // A running hypervisor proves the firmware switch is on even if the flag is hidden from us
        let virt_on = f.firmware_virt_enabled || f.hypervisor_present;
        let vendor = cpuid_vendor();
        let dma_protection = f.available_props.contains(&3);
        let details = format!(
            "firmware virtualization flag={}; hypervisor present={}; DMA protection={}; VBS status={}; PowerShell: {}",
            f.firmware_virt_enabled, f.hypervisor_present, dma_protection, f.vbs_status, f.raw
        );
        Ok(BiosVirtualization {
            vt_x_enabled: virt_on && vendor != "AuthenticAMD",
            svm_enabled: virt_on && vendor == "AuthenticAMD",
            // Kernel DMA Protection is only active when the IOMMU (VT-d / AMD-Vi) is on
            vt_d_enabled: dma_protection && vendor != "AuthenticAMD",
            iommu_enabled: dma_protection,
            tpm_enabled: f.tpm_present,
            secure_boot_enabled: f.secure_boot,
            details,
        })
    }

    #[cfg(target_os = "windows")]
    fn windows_from_facts(f: &WinFacts) -> WindowsVirtualization {
        let vbs_enabled = f.vbs_status == 2;
        let hvci_enabled = f.services_running.contains(&2) || Self::hvci_registry_enabled();
        let wsl2 = f.feature_vm_platform && f.feature_wsl;
        let core_isolation_enabled = vbs_enabled || hvci_enabled;
        WindowsVirtualization {
            // "Hyper-V" here means the hypervisor is actually active: the optional feature,
            // or VBS/WSL2/etc. having launched it
            hyper_v_enabled: f.feature_hyper_v || f.hypervisor_present,
            vbs_enabled,
            hvci_enabled,
            wsl_enabled: f.feature_wsl || f.feature_vm_platform,
            wsl_version: if wsl2 { Some("2".to_string()) } else { None },
            core_isolation_enabled,
            memory_integrity_enabled: hvci_enabled,
            virtual_machine_platform_enabled: f.feature_vm_platform,
            windows_hypervisor_platform_enabled: f.feature_hypervisor_platform,
            details: format!(
                "hypervisor present={}; VBS status={}; services running={:?}; features: HyperV={} VMP={} WHP={} WSL={}",
                f.hypervisor_present, f.vbs_status, f.services_running,
                f.feature_hyper_v, f.feature_vm_platform, f.feature_hypervisor_platform, f.feature_wsl
            ),
        }
    }

    /// Memory Integrity toggle: HKLM\...\DeviceGuard\Scenarios\HypervisorEnforcedCodeIntegrity\Enabled
    #[cfg(target_os = "windows")]
    fn hvci_registry_enabled() -> bool {
        use std::os::windows::process::CommandExt;
        Command::new("reg")
            .args(["query", r"HKLM\SYSTEM\CurrentControlSet\Control\DeviceGuard\Scenarios\HypervisorEnforcedCodeIntegrity", "/v", "Enabled"])
            .creation_flags(0x0800_0000)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("0x1"))
            .unwrap_or(false)
    }

    #[cfg(target_os = "linux")]
    fn detect_bios_virtualization_linux() -> Result<BiosVirtualization> {
        let mut details = String::new();
        let mut vt_x_enabled = false;
        let mut svm_enabled = false;
        let mut vt_d_enabled = false;
        let mut iommu_enabled = false;
        let mut tpm_enabled = false;
        let mut secure_boot_enabled = false;

        // Check CPU flags
        if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
            if cpuinfo.contains("vmx") {
                vt_x_enabled = true;
                details.push_str("VMX (VT-x) found in cpuinfo; ");
            }
            if cpuinfo.contains("svm") {
                svm_enabled = true;
                details.push_str("SVM (AMD-V) found in cpuinfo; ");
            }
            if cpuinfo.contains("vmx") && cpuinfo.contains("ept") {
                vt_d_enabled = true;
                details.push_str("EPT (VT-d) found in cpuinfo; ");
            }
        }

        // Check IOMMU
        if std::path::Path::new("/sys/kernel/iommu_groups").exists() {
            iommu_enabled = true;
            details.push_str("IOMMU groups found; ");
        }

        // Check kernel commandline for IOMMU
        if let Ok(cmdline) = std::fs::read_to_string("/proc/cmdline") {
            if cmdline.contains("iommu=on") || cmdline.contains("intel_iommu=on") || cmdline.contains("amd_iommu=on") {
                iommu_enabled = true;
                details.push_str("IOMMU enabled in kernel cmdline; ");
            }
            if cmdline.contains("iommu=off") || cmdline.contains("intel_iommu=off") || cmdline.contains("amd_iommu=off") {
                iommu_enabled = false;
                details.push_str("IOMMU disabled in kernel cmdline; ");
            }
        }

        // Check TPM
        if std::path::Path::new("/dev/tpm0").exists() || std::path::Path::new("/dev/tpmrm0").exists() {
            tpm_enabled = true;
            details.push_str("TPM device found; ");
        }

        // Check Secure Boot
        if let Ok(output) = Command::new("mokutil")
            .args(["--sb-state"])
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            if output_str.contains("enabled") {
                secure_boot_enabled = true;
                details.push_str("Secure Boot enabled; ");
            }
        } else if let Ok(secure_boot) = std::fs::read_to_string("/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c") {
            if secure_boot.len() > 4 && secure_boot.as_bytes()[4] == 1 {
                secure_boot_enabled = true;
                details.push_str("Secure Boot enabled (efivar); ");
            }
        }

        Ok(BiosVirtualization {
            vt_x_enabled,
            svm_enabled,
            vt_d_enabled,
            iommu_enabled,
            tpm_enabled,
            secure_boot_enabled,
            details,
        })
    }

    fn detect_linux_virtualization() -> Result<LinuxVirtualization> {
        let mut details = String::new();
        let mut kvm_enabled = false;
        let mut kvm_intel_loaded = false;
        let mut kvm_amd_loaded = false;
        let mut vmx_svm_in_cpuinfo = false;
        let mut nested_virt_enabled = false;
        let mut kernel_cmdline_virt_disabled = false;
        let mut systemd_detect_virt = "none".to_string();

        // Check KVM
        if std::path::Path::new("/dev/kvm").exists() {
            kvm_enabled = true;
            details.push_str("/dev/kvm exists; ");
        }

        // Check loaded modules
        if let Ok(modules) = std::fs::read_to_string("/proc/modules") {
            if modules.contains("kvm_intel") {
                kvm_intel_loaded = true;
                details.push_str("kvm_intel loaded; ");
            }
            if modules.contains("kvm_amd") {
                kvm_amd_loaded = true;
                details.push_str("kvm_amd loaded; ");
            }
        }

        // Check CPU flags
        if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
            if cpuinfo.contains("vmx") || cpuinfo.contains("svm") {
                vmx_svm_in_cpuinfo = true;
                details.push_str("VMX/SVM in cpuinfo; ");
            }
        }

        // Check nested virtualization
        if let Ok(nested) = std::fs::read_to_string("/sys/module/kvm_intel/parameters/nested") {
            if nested.trim() == "Y" || nested.trim() == "1" {
                nested_virt_enabled = true;
                details.push_str("Nested virt enabled (Intel); ");
            }
        }
        if let Ok(nested) = std::fs::read_to_string("/sys/module/kvm_amd/parameters/nested") {
            if nested.trim() == "Y" || nested.trim() == "1" {
                nested_virt_enabled = true;
                details.push_str("Nested virt enabled (AMD); ");
            }
        }

        // Check kernel cmdline for disabled virtualization
        if let Ok(cmdline) = std::fs::read_to_string("/proc/cmdline") {
            if cmdline.contains("kvm_intel.enable_virt_at_load=0") || 
               cmdline.contains("kvm_amd.enable_virt_at_load=0") ||
               cmdline.contains("nokvm") ||
               cmdline.contains("kvm=off") {
                kernel_cmdline_virt_disabled = true;
                details.push_str("Virtualization disabled in kernel cmdline; ");
            }
        }

        // systemd-detect-virt
        if let Ok(output) = Command::new("systemd-detect-virt")
            .output()
        {
            systemd_detect_virt = String::from_utf8_lossy(&output.stdout).trim().to_string();
            details.push_str(&format!("systemd-detect-virt: {}; ", systemd_detect_virt));
        }

        Ok(LinuxVirtualization {
            kvm_enabled,
            kvm_intel_loaded,
            kvm_amd_loaded,
            vmx_svm_in_cpuinfo,
            nested_virt_enabled,
            kernel_cmdline_virt_disabled,
            systemd_detect_virt,
            details,
        })
    }

    fn generate_recommendations(
        bios: &BiosVirtualization,
        windows: &Option<WindowsVirtualization>,
        linux: &Option<LinuxVirtualization>,
    ) -> Vec<String> {
        let mut recommendations = Vec::new();

        // BIOS recommendations
        if !bios.vt_x_enabled && !bios.svm_enabled {
            recommendations.push("BIOS: Virtualization (VT-x/SVM) appears disabled. Enable in BIOS for VM support.".to_string());
        } else {
            recommendations.push("BIOS: Virtualization extensions are enabled.".to_string());
        }

        if bios.vt_d_enabled || bios.iommu_enabled {
            recommendations.push("BIOS: VT-d/IOMMU enabled - good for device passthrough.".to_string());
        }

        // Windows recommendations
        if let Some(win) = windows {
            if win.hyper_v_enabled {
                recommendations.push("Windows: Hyper-V is enabled. This adds virtualization overhead. Disable for maximum gaming performance.".to_string());
            }
            
            if win.vbs_enabled {
                recommendations.push("Windows: VBS (Virtualization-Based Security) is enabled. This causes CPU latency spikes in games. Disable via Core Isolation settings.".to_string());
            }
            
            if win.hvci_enabled || win.memory_integrity_enabled {
                recommendations.push("Windows: Memory Integrity (HVCI) is enabled. This significantly impacts 1% lows in games. Disable in Windows Security > Core Isolation.".to_string());
            }
            
            if win.wsl_enabled {
                recommendations.push("Windows: WSL is enabled. Requires virtualization. Disable if not needed for development.".to_string());
            }
            
            if win.virtual_machine_platform_enabled {
                recommendations.push("Windows: Virtual Machine Platform is enabled. Disable if not using WSL2 or VMs.".to_string());
            }
            
            if !win.hyper_v_enabled && !win.vbs_enabled && !win.hvci_enabled && !win.wsl_enabled {
                recommendations.push("Windows: All virtualization features disabled - optimal for gaming latency.".to_string());
            }
        }

        // Linux recommendations
        if let Some(lnx) = linux {
            if lnx.kvm_enabled {
                recommendations.push("Linux: KVM is enabled. For maximum gaming performance, consider disabling KVM modules (modprobe -r kvm_intel/kvm_amd) or adding 'kvm_intel.enable_virt_at_load=0' to kernel cmdline.".to_string());
            }
            
            if lnx.kernel_cmdline_virt_disabled {
                recommendations.push("Linux: Virtualization disabled via kernel command line - optimal for gaming.".to_string());
            }
            
            if lnx.systemd_detect_virt != "none" {
                recommendations.push(format!("Linux: Running inside a VM ({}). Results may not reflect bare-metal performance.", lnx.systemd_detect_virt));
            }
        }

        // General gaming recommendations
        recommendations.push("Gaming: For lowest latency, disable all virtualization in BIOS and OS. This prevents VBS/HVCI overhead and eliminates VM exit latency.".to_string());
        recommendations.push("Gaming: Disable HPET in BIOS if available (use Invariant TSC instead).".to_string());
        recommendations.push("Gaming: Set Windows power plan to 'High Performance' or 'Ultimate Performance'. Disable CPU parking.".to_string());
        recommendations.push("Gaming: Disable Spectre/Meltdown mitigations if security not a concern (kernel cmdline: mitigations=off on Linux, registry on Windows).".to_string());

        recommendations
    }

    /// Get instructions for disabling virtualization on Windows
    pub fn get_windows_disable_instructions() -> Vec<String> {
        vec![
            "1. Disable Memory Integrity (HVCI):".to_string(),
            "   - Open Windows Security > Device Security > Core Isolation details".to_string(),
            "   - Turn off 'Memory integrity'".to_string(),
            "   - Restart".to_string(),
            "".to_string(),
            "2. Disable VBS (Virtualization-Based Security):".to_string(),
            "   - Run 'msinfo32' and check 'Virtualization-based security'".to_string(),
            "   - If running, disable via Group Policy:".to_string(),
            "     gpedit.msc > Computer Config > Admin Templates > System > Device Guard".to_string(),
            "     Turn off 'Turn on Virtualization Based Security'".to_string(),
            "".to_string(),
            "3. Disable Hyper-V:".to_string(),
            "   - Control Panel > Programs > Turn Windows features on/off".to_string(),
            "   - Uncheck 'Hyper-V' and 'Virtual Machine Platform'".to_string(),
            "   - Or: dism /online /disable-feature /featurename:Microsoft-Hyper-V-All".to_string(),
            "".to_string(),
            "4. Disable WSL (if not needed):".to_string(),
            "   - wsl --unregister <distro>".to_string(),
            "   - Turn off 'Windows Subsystem for Linux' in Windows Features".to_string(),
            "".to_string(),
            "5. BIOS/UEFI (most effective):".to_string(),
            "   - Enter BIOS/UEFI (F2, Del, F12 at boot)".to_string(),
            "   - Disable 'Intel Virtualization Technology (VT-x)' or 'AMD SVM'".to_string(),
            "   - Disable 'VT-d' / 'IOMMU' if not using device passthrough".to_string(),
            "   - Save and exit".to_string(),
        ]
    }

    /// Get instructions for disabling virtualization on Linux
    pub fn get_linux_disable_instructions() -> Vec<String> {
        vec![
            "1. Disable KVM modules (temporary):".to_string(),
            "   sudo modprobe -r kvm_intel  # or kvm_amd".to_string(),
            "".to_string(),
            "2. Disable KVM permanently (kernel cmdline):".to_string(),
            "   Edit /etc/default/grub:".to_string(),
            "   GRUB_CMDLINE_LINUX_DEFAULT=\"... kvm_intel.enable_virt_at_load=0\"".to_string(),
            "   # or for AMD: kvm_amd.enable_virt_at_load=0".to_string(),
            "   sudo update-grub".to_string(),
            "".to_string(),
            "3. Disable virtualization in BIOS/UEFI:".to_string(),
            "   - Enter BIOS/UEFI at boot".to_string(),
            "   - Disable 'Intel Virtualization Technology' or 'AMD SVM'".to_string(),
            "   - Disable 'VT-d' / 'IOMMU' if not needed".to_string(),
            "".to_string(),
            "4. Blacklist KVM modules:".to_string(),
            "   echo 'blacklist kvm_intel' | sudo tee /etc/modprobe.d/blacklist-kvm.conf".to_string(),
            "   echo 'blacklist kvm_amd' | sudo tee -a /etc/modprobe.d/blacklist-kvm.conf".to_string(),
            "   sudo update-initramfs -u".to_string(),
            "".to_string(),
            "5. Disable mitigations (if security not a concern):".to_string(),
            "   Add to kernel cmdline: mitigations=off".to_string(),
            "   Or: echo 'off' | sudo tee /sys/devices/system/cpu/vulnerabilities/*/mitigation".to_string(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_platform() {
        let platform = VirtualizationDetector::detect_platform();
        assert_ne!(platform, Platform::Unknown);
    }

    #[test]
    fn parses_vbs_running_sample() {
        // Shape of a real machine: VBS running, hypervisor active, no optional features listed
        let out = r#"{"vbs":2,"running":[],"avail":[1,2,3,5,6,7],"hv":true,"feat":[],"secureboot":1,"tpm":false}"#;
        let f = parse_win_facts(out).unwrap();
        assert_eq!(f.vbs_status, 2);
        assert!(f.hypervisor_present && f.secure_boot);
        assert!(f.available_props.contains(&3));
        assert!(f.services_running.is_empty());
    }

    #[test]
    fn parses_scalar_and_single_object_quirks() {
        // PowerShell collapses 1-element arrays in some versions
        let out = "WARNING: something\n{\"vbs\":1,\"running\":2,\"avail\":3,\"hv\":false,\"feat\":{\"n\":\"VirtualMachinePlatform\",\"s\":1},\"secureboot\":0,\"tpm\":true}";
        let f = parse_win_facts(out).unwrap();
        assert_eq!(f.services_running, vec![2]);
        assert_eq!(f.available_props, vec![3]);
        assert!(f.feature_vm_platform && !f.feature_wsl && f.tpm_present && !f.secure_boot);
    }

    #[test]
    fn tpm_is_found_without_admin_rights() {
        // Not elevated: Win32_Tpm returns nothing (tpm=false) but the TPM's PnP device is present
        let out = r#"{"vbs":2,"running":[],"avail":[],"hv":true,"feat":[],"secureboot":1,"tpm":false,"tpmdev":true}"#;
        assert!(parse_win_facts(out).unwrap().tpm_present);
        // Neither source sees one (or an older script without the field)
        let none = r#"{"vbs":0,"running":[],"avail":[],"hv":false,"feat":[],"secureboot":0,"tpm":false,"tpmdev":false}"#;
        assert!(!parse_win_facts(none).unwrap().tpm_present);
        let old = r#"{"vbs":0,"running":[],"avail":[],"hv":false,"feat":[],"secureboot":0,"tpm":false}"#;
        assert!(!parse_win_facts(old).unwrap().tpm_present);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_win_facts("").is_none());
        assert!(parse_win_facts("not json").is_none());
    }
}
