@echo off
rem Builds the release exe and copies it to dist\LatencyTester.exe (double-click to run the GUI)
setlocal
cd /d "%~dp0"
where cargo >nul 2>nul || (echo Rust is not installed. Get it from https://rustup.rs & exit /b 1)
cargo build --release || exit /b 1
if not exist dist mkdir dist
copy /y target\release\latency-tester.exe dist\LatencyTester.exe >nul
if errorlevel 1 (
  echo.
  echo Could not overwrite dist\LatencyTester.exe - it is probably still running.
  echo Close Latency Tester ^(check Task Manager^) and run build.bat again.
  echo The fresh build is at target\release\latency-tester.exe
  exit /b 1
)
rem Optional: LibreHardwareMonitor's library for board / VRM / memory / fan / power sensors (MPL-2.0).
rem The app works without it; the Dashboard also has a Download button.
if not exist dist\LibreHardwareMonitor\LibreHardwareMonitorLib.dll (
  echo Fetching LibreHardwareMonitor library for the sensors...
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\get-librehardwaremonitor.ps1 -Target dist || echo   (skipped: no download possible, sensors fall back to the LibreHardwareMonitor app or ACPI)
)
echo.
echo Done: %~dp0dist\LatencyTester.exe
echo Run it with a double-click, or "dist\LatencyTester.exe --cli" from a terminal.
