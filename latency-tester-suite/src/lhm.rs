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

/// Is this process running with administrator rights?
#[cfg(target_os = "windows")]
pub fn is_admin() -> bool {
    // `net session` only succeeds for an administrator; cheap and needs no extra API features
    crate::sensors::hidden_command("net")
        .arg("session")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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

#[cfg(test)]
mod tests {
    #[test]
    fn script_targets_the_library_folder() {
        assert!(super::FETCH_SCRIPT.contains("LibreHardwareMonitorLib.dll"));
        assert!(super::FETCH_SCRIPT.contains("LibreHardwareMonitor/LibreHardwareMonitor"));
        assert!(super::target_dir().unwrap().ends_with("LibreHardwareMonitor"));
    }
}
