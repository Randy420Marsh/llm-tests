# Downloads the LibreHardwareMonitor library (https://github.com/LibreHardwareMonitor/LibreHardwareMonitor,
# MPL-2.0) and puts its DLLs into <target>\LibreHardwareMonitor, where Latency Tester loads it to read
# CPU, board (VRM, chipset, fans, voltages), memory, drive, GPU and PSU sensors.
#   powershell -ExecutionPolicy Bypass -File get-librehardwaremonitor.ps1 [-Target <folder of LatencyTester.exe>]
param([string]$Target = (Join-Path $PSScriptRoot '..\dist'))
$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$dest = Join-Path $Target 'LibreHardwareMonitor'
$api = 'https://api.github.com/repos/LibreHardwareMonitor/LibreHardwareMonitor/releases/latest'
$rel = Invoke-RestMethod -Uri $api -Headers @{ 'User-Agent' = 'latency-tester' }
# Windows PowerShell 5.1 runs on .NET Framework, so the net472 build is the one it can load
$zips = @($rel.assets | Where-Object { $_.name -like '*.zip' })
$asset = $zips | Where-Object { $_.name -match 'net4' } | Select-Object -First 1
if (-not $asset) { $asset = $zips | Select-Object -First 1 }
if (-not $asset) { throw "No zip asset in release $($rel.tag_name)" }
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("lhm_" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
$zip = Join-Path $tmp $asset.name
Write-Host "Downloading $($asset.name) ($($rel.tag_name))"
Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $zip -UseBasicParsing
Expand-Archive -Path $zip -DestinationPath (Join-Path $tmp 'x') -Force
New-Item -ItemType Directory -Path $dest -Force | Out-Null
Get-ChildItem (Join-Path $tmp 'x') -Recurse -Filter *.dll | Copy-Item -Destination $dest -Force
Get-ChildItem (Join-Path $tmp 'x') -Recurse -Include LICENSE*, *.txt | Copy-Item -Destination $dest -Force -ErrorAction SilentlyContinue
Remove-Item $tmp -Recurse -Force
if (-not (Test-Path (Join-Path $dest 'LibreHardwareMonitorLib.dll'))) { throw "LibreHardwareMonitorLib.dll was not in $($asset.name)" }
Write-Host "LibreHardwareMonitor $($rel.tag_name) installed in $dest"
