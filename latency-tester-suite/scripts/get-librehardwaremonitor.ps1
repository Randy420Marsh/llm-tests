# Downloads LibreHardwareMonitor (https://github.com/LibreHardwareMonitor/LibreHardwareMonitor, MPL-2.0)
# into <target>\LibreHardwareMonitor, where Latency Tester loads its library to read CPU, board (VRM,
# chipset, fans, voltages), memory, drive, GPU and PSU sensors.
#   powershell -ExecutionPolicy Bypass -File get-librehardwaremonitor.ps1 [-Target <folder of LatencyTester.exe>]
# Since v0.9.5 the CPU / board / memory sensors go through the PawnIO driver: LibreHardwareMonitor.exe
# (kept in the same folder) installs it on its first start, or use "Install PawnIO" on the Dashboard.
param([string]$Target = (Join-Path $PSScriptRoot '..\dist'))
$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$dest = Join-Path $Target 'LibreHardwareMonitor'
$api = 'https://api.github.com/repos/LibreHardwareMonitor/LibreHardwareMonitor/releases/latest'
$rel = Invoke-RestMethod -Uri $api -Headers @{ 'User-Agent' = 'latency-tester' }
# Windows PowerShell 5.1 runs on .NET Framework, so only the .NET Framework build can be loaded:
# "LibreHardwareMonitor.zip" (older releases: "...net472.zip"), not "LibreHardwareMonitor.NET.10.zip"
$zips = @($rel.assets | Where-Object { $_.name -like '*.zip' })
$asset = $zips | Where-Object { $_.name -match 'net4' } | Select-Object -First 1
if (-not $asset) { $asset = $zips | Where-Object { $_.name -notmatch '\.NET\.?\d|net\d' } | Select-Object -First 1 }
if (-not $asset) { throw "No .NET Framework zip in release $($rel.tag_name): $(($zips | ForEach-Object name) -join ', ')" }
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("lhm_" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
$zip = Join-Path $tmp $asset.name
Write-Host "Downloading $($asset.name) ($($rel.tag_name))"
Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $zip -UseBasicParsing
Expand-Archive -Path $zip -DestinationPath (Join-Path $tmp 'x') -Force
# replace whatever was there (an earlier download may be the .NET 10 build)
if (Test-Path $dest) { Remove-Item (Join-Path $dest '*') -Recurse -Force -ErrorAction SilentlyContinue }
New-Item -ItemType Directory -Path $dest -Force | Out-Null
Copy-Item (Join-Path (Join-Path $tmp 'x') '*') -Destination $dest -Recurse -Force
Remove-Item $tmp -Recurse -Force
Get-ChildItem $dest -Recurse -File | ForEach-Object { try { Unblock-File $_.FullName } catch {} }
if (-not (Test-Path (Join-Path $dest 'LibreHardwareMonitorLib.dll'))) { throw "LibreHardwareMonitorLib.dll was not in $($asset.name)" }
# prove that this PowerShell can load it (the sensor helper runs the same way)
try {
  [void][Reflection.Assembly]::UnsafeLoadFrom((Join-Path $dest 'LibreHardwareMonitorLib.dll'))
  [void][LibreHardwareMonitor.Hardware.Computer]
} catch { throw "LibreHardwareMonitor $($rel.tag_name) was downloaded but cannot be loaded: $($_.Exception.GetBaseException().Message)" }
$pawn = ''
foreach ($k in 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO', 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO') {
  if (-not $pawn) { $pawn = [string](Get-ItemProperty $k -ErrorAction SilentlyContinue).DisplayVersion }
}
if ($pawn) { $p = "PawnIO $pawn is installed" } else { $p = 'PawnIO driver is NOT installed yet (needed for CPU, board and memory sensors): press Install PawnIO' }
Write-Host "LibreHardwareMonitor $($rel.tag_name) installed in $dest; $p"
