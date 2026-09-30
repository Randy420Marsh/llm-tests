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

pub struct VirtualizationDetector;

impl VirtualizationDetector {
    pub fn detect() -> Result<VirtualizationStatus> {
        let platform = Self::detect_platform();
        
        let bios = Self::detect_bios_virtualization()?;
        let windows = if platform == Platform::Windows {
            Some(Self::detect_windows_virtualization()?)
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

    fn detect_bios_virtualization() -> Result<BiosVirtualization> {
        #[cfg(target_os = "windows")]
        {
            Self::detect_bios_virtualization_windows()
        }
        
        #[cfg(target_os = "linux")]
        {
            Self::detect_bios_virtualization_linux()
        }
        
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
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
    fn detect_bios_virtualization_windows() -> Result<BiosVirtualization> {
        let mut details = String::new();
        let mut vt_x_enabled = false;
        let mut svm_enabled = false;
        let mut vt_d_enabled = false;
        let mut iommu_enabled = false;
        let mut tpm_enabled = false;
        let mut secure_boot_enabled = false;

        // Check CPU virtualization support via WMI
        if let Ok(output) = Command::new("wmic")
            .args(["cpu", "get", "VirtualizationFirmwareEnabled,SecondLevelAddressTranslationExtensions,VMMonitorModeExtensions"])
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            details.push_str(&format!("WMI CPU: {}; ", output_str.trim()));
            
            if output_str.contains("TRUE") {
                vt_x_enabled = true;
                svm_enabled = true; // WMI doesn't distinguish
            }
        }

        // Check for VT-d / IOMMU
        if let Ok(output) = Command::new("wmic")
            .args(["path", "Win32_DeviceGuard", "get", "VirtualizationBasedSecurityStatus"])
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            details.push_str(&format!("DeviceGuard: {}; ", output_str.trim()));
        }

        // Check TPM
        if let Ok(output) = Command::new("wmic")
            .args(["path", "Win32_Tpm", "get", "SpecVersion,IsEnabled_InitialValue"])
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            details.push_str(&format!("TPM: {}; ", output_str.trim()));
            if output_str.contains("TRUE") {
                tpm_enabled = true;
            }
        }

        // Check Secure Boot
        if let Ok(output) = Command::new("powershell")
            .args(["-Command", "Confirm-SecureBootUEFI"])
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            details.push_str(&format!("SecureBoot: {}; ", output_str.trim()));
            if output_str.contains("True") {
                secure_boot_enabled = true;
            }
        }

        // Check IOMMU via kernel DMA protection
        if let Ok(output) = Command::new("powershell")
            .args(["-Command", "Get-ItemProperty -Path 'HKLM:\\SYSTEM\\CurrentControlSet\\Control\\DeviceGuard\\Scenarios\\SystemGuard' -Name 'Enabled' -ErrorAction SilentlyContinue"])
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            details.push_str(&format!("SystemGuard: {}; ", output_str.trim()));
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

    fn detect_windows_virtualization() -> Result<WindowsVirtualization> {
        let mut details = String::new();
        let mut hyper_v_enabled = false;
        let mut vbs_enabled = false;
        let mut hvci_enabled = false;
        let mut wsl_enabled = false;
        let mut wsl_version = None;
        let mut memory_integrity_enabled = false;
        let mut virtual_machine_platform_enabled = false;
        let mut windows_hypervisor_platform_enabled = false;

        // Check Hyper-V
        if let Ok(output) = Command::new("powershell")
            .args(["-Command", "Get-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V-All | Select-Object State"])
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            details.push_str(&format!("Hyper-V: {}; ", output_str.trim()));
            if output_str.contains("Enabled") {
                hyper_v_enabled = true;
            }
        }

        // Check VBS and HVCI
        if let Ok(output) = Command::new("powershell")
            .args(["-Command", "Get-CimInstance -Namespace root\\Microsoft\\Windows\\DeviceGuard -ClassName DeviceGuardSecurityProperties | Select-Object VirtualizationBasedSecurityStatus, RequiredSecurityProperties"])
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            details.push_str(&format!("DeviceGuard: {}; ", output_str.trim()));
            
            // VBS status: 0=not supported, 1=supported but not running, 2=running
            if output_str.contains("VirtualizationBasedSecurityStatus") {
                if output_str.contains("2") {
                    vbs_enabled = true;
                }
            }
            // HVCI: RequiredSecurityProperties = 1 means HVCI enabled
            if output_str.contains("RequiredSecurityProperties") {
                if output_str.contains("1") {
                    hvci_enabled = true;
                    memory_integrity_enabled = true;
                }
            }
        }

        // Check Core Isolation / Memory Integrity via registry
        if let Ok(output) = Command::new("reg")
            .args(["query", "HKLM\\SYSTEM\\CurrentControlSet\\Control\\DeviceGuard\\Scenarios\\HypervisorEnforcedCodeIntegrity", "/v", "Enabled"])
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            details.push_str(&format!("HVCI Registry: {}; ", output_str.trim()));
            if output_str.contains("0x1") {
                hvci_enabled = true;
                memory_integrity_enabled = true;
            }
        }

        // Check WSL
        if let Ok(output) = Command::new("wsl")
            .args(["--status"])
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            details.push_str(&format!("WSL: {}; ", output_str.trim()));
            if output_str.contains("WSL 2") {
                wsl_enabled = true;
                wsl_version = Some("2".to_string());
            } else if output_str.contains("WSL 1") {
                wsl_enabled = true;
                wsl_version = Some("1".to_string());
            }
        }

        // Check Virtual Machine Platform
        if let Ok(output) = Command::new("powershell")
            .args(["-Command", "Get-WindowsOptionalFeature -Online -FeatureName VirtualMachinePlatform | Select-Object State"])
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            details.push_str(&format!("VirtualMachinePlatform: {}; ", output_str.trim()));
            if output_str.contains("Enabled") {
                virtual_machine_platform_enabled = true;
            }
        }

        // Check Windows Hypervisor Platform
        if let Ok(output) = Command::new("powershell")
            .args(["-Command", "Get-WindowsOptionalFeature -Online -FeatureName Windows-Hypervisor-Platform | Select-Object State"])
            .output()
        {
            let output_str = String::from_utf8_lossy(&output.stdout);
            details.push_str(&format!("WindowsHypervisorPlatform: {}; ", output_str.trim()));
            if output_str.contains("Enabled") {
                windows_hypervisor_platform_enabled = true;
            }
        }

        // Core Isolation
        let core_isolation_enabled = vbs_enabled || hvci_enabled;

        Ok(WindowsVirtualization {
            hyper_v_enabled,
            vbs_enabled,
            hvci_enabled,
            wsl_enabled,
            wsl_version,
            core_isolation_enabled,
            memory_integrity_enabled,
            virtual_machine_platform_enabled,
            windows_hypervisor_platform_enabled,
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
}