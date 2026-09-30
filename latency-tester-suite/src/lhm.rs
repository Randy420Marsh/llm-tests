//! LibreHardwareMonitor (https://github.com/LibreHardwareMonitor/LibreHardwareMonitor, MPL-2.0) on Windows:
//! where its library is, fetching it, and restarting the app with administrator rights (its driver,
//! and so the CPU, board and memory sensors, only load for an administrator).

use std::path::PathBuf;

/// The download script, also shipped in `scripts/`
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub const FETCH_SCRIPT: &str = include_str!("../scripts/get-librehardwaremonitor.ps1");

/// Folder next to the exe that holds (or will hold) LibreHardwareMonitorLib.dll
pub fn target_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(|d| d.join("LibreHardwareMonitor"))
}

/// The folder, if the library is actually there
pub fn dir() -> Option<PathBuf> {
    target_dir().filter(|d| d.join("LibreHardwareMonitorLib.dll").exists())
}

/// Download the latest release into `target_dir()` (blocking; run it on a worker thread)
#[cfg(target_os = "windows")]
pub fn fetch() -> Result<String, String> {
    let exe_dir = std::env::current_exe().map_err(|e| e.to_string())?.parent().map(|d| d.to_path_buf()).ok_or("no exe folder")?;
    let script = std::env::temp_dir().join("lts_get_lhm.ps1");
    std::fs::write(&script, FETCH_SCRIPT).map_err(|e| e.to_string())?;
    let out = crate::sensors::hidden_command("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .arg("-Target")
        .arg(&exe_dir)
        .output()
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&script);
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() { Ok(text.trim().to_string()) } else { Err(text.trim().to_string()) }
}

#[cfg(not(target_os = "windows"))]
pub fn fetch() -> Result<String, String> {
    Err("LibreHardwareMonitor is only used on Windows (Linux reads hwmon directly)".into())
}

/// Is this process running with administrator rights? (checked once)
#[cfg(target_os = "windows")]
pub fn is_admin() -> bool {
    static ADMIN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ADMIN.get_or_init(|| {
        // `net session` only succeeds for an administrator; cheap and needs no extra API features
        crate::sensors::hidden_command("net")
            .arg("session")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

#[cfg(not(target_os = "windows"))]
pub fn is_admin() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Start a second copy of the app elevated (UAC prompt); the caller closes this one if it worked
#[cfg(target_os = "windows")]
pub fn restart_as_admin() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let status = crate::sensors::hidden_command("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!("Start-Process -FilePath '{}' -Verb RunAs", exe.display().to_string().replace('\'', "''")))
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err("the administrator prompt was cancelled".into()) }
}

#[cfg(not(target_os = "windows"))]
pub fn restart_as_admin() -> Result<(), String> {
    Err("start the app with sudo to read root-only sensors (e.g. RAPL power)".into())
}

/// Installs the PawnIO driver, `scripts/install-pawnio.ps1`
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub const PAWNIO_SCRIPT: &str = include_str!("../scripts/install-pawnio.ps1");

/// Installed PawnIO driver version, from its uninstall entry (None = not installed)
#[cfg(target_os = "windows")]
pub fn pawnio_version() -> Option<String> {
    for key in [
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO",
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO",
    ] {
        let out = crate::sensors::hidden_command("reg").args(["query", key, "/v", "DisplayVersion"]).output().ok()?;
        if let Some(v) = parse_reg_value(&String::from_utf8_lossy(&out.stdout), "DisplayVersion") {
            return Some(v);
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
pub fn pawnio_version() -> Option<String> {
    None
}

/// The data of `name` in `reg query` output ("    DisplayVersion    REG_SZ    2.0.1.0")
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn parse_reg_value(out: &str, name: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        (it.next()? == name).then_some(())?;
        it.next()?; // REG_SZ
        let v = it.collect::<Vec<_>>().join(" ");
        (!v.is_empty()).then_some(v)
    })
}

/// Install PawnIO with the setup inside LibreHardwareMonitor.exe (blocking; UAC prompt when this
/// process is not elevated)
#[cfg(target_os = "windows")]
pub fn install_pawnio() -> Result<String, String> {
    let lhm = dir().ok_or("LibreHardwareMonitor is not downloaded yet: press Download LibreHardwareMonitor first")?;
    let script = std::env::temp_dir().join("lts_install_pawnio.ps1");
    std::fs::write(&script, PAWNIO_SCRIPT).map_err(|e| e.to_string())?;
    let quote = |p: &std::path::Path| p.display().to_string().replace('\'', "''");
    let result = if is_admin() {
        crate::sensors::hidden_command("powershell")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&script)
            .arg("-LhmDir")
            .arg(&lhm)
            .output()
            .map_err(|e| e.to_string())
            .map(|o| (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))))
    } else {
        // elevated child; its output is not visible here, so the result is read back from the registry
        crate::sensors::hidden_command("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command"])
            .arg(format!(
                "$p = Start-Process powershell -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','\"{}\"','-LhmDir','\"{}\"'; exit $p.ExitCode",
                quote(&script),
                quote(&lhm)
            ))
            .status()
            .map_err(|e| e.to_string())
            .map(|s| (s.success(), String::new()))
    };
    let _ = std::fs::remove_file(&script);
    let (ok, text) = result?;
    match pawnio_version() {
        Some(v) => Ok(format!("PawnIO {} installed · used from the next sensor reading on", v)),
        None if ok => Err("PawnIO setup finished but PawnIO is not registered as installed".into()),
        None => Err(if text.trim().is_empty() { "PawnIO setup failed or was cancelled".into() } else { text.trim().to_string() }),
    }
}

#[cfg(not(target_os = "windows"))]
pub fn install_pawnio() -> Result<String, String> {
    Err("PawnIO is a Windows driver".into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_the_registry_value() {
        let out = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\...\\PawnIO\r\n    DisplayVersion    REG_SZ    2.0.1.0\r\n\r\n";
        assert_eq!(super::parse_reg_value(out, "DisplayVersion").as_deref(), Some("2.0.1.0"));
        assert_eq!(super::parse_reg_value("ERROR: not found", "DisplayVersion"), None);
        assert!(super::PAWNIO_SCRIPT.contains("PawnIO_setup.exe") && super::PAWNIO_SCRIPT.contains("-install"));
    }

    #[test]
    fn fetch_picks_the_net_framework_build() {
        // v0.9.6 ships "LibreHardwareMonitor.zip" (.NET Framework) and "LibreHardwareMonitor.NET.10.zip"
        assert!(super::FETCH_SCRIPT.contains(r"-notmatch '\.NET\.?\d|net\d'"));
        assert!(super::FETCH_SCRIPT.contains("UnsafeLoadFrom"));
    }

    #[test]
    fn script_targets_the_library_folder() {
        assert!(super::FETCH_SCRIPT.contains("LibreHardwareMonitorLib.dll"));
        assert!(super::FETCH_SCRIPT.contains("LibreHardwareMonitor/LibreHardwareMonitor"));
        assert!(super::target_dir().unwrap().ends_with("LibreHardwareMonitor"));
    }
}
