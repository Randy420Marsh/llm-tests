# Installs the PawnIO driver (https://pawnio.eu, by namazso) that LibreHardwareMonitor 0.9.5 and later
# use to read CPU (MSR), motherboard (Super I/O: VRM, fans, voltages) and memory (SPD) sensors.
# It runs the PawnIO setup that ships inside LibreHardwareMonitor.exe, exactly as LibreHardwareMonitor
# does on its first start ("PawnIO is not installed, do you want to install it?"). Needs administrator.
#   powershell -ExecutionPolicy Bypass -File install-pawnio.ps1 -LhmDir <folder with LibreHardwareMonitor.exe>
param([Parameter(Mandatory = $true)][string]$LhmDir)
$ErrorActionPreference = 'Stop'
$exe = Join-Path $LhmDir 'LibreHardwareMonitor.exe'
if (-not (Test-Path $exe)) { throw "LibreHardwareMonitor.exe is not in ${LhmDir}; press Update LibreHardwareMonitor first" }
$asm = [Reflection.Assembly]::UnsafeLoadFrom($exe)
$res = $asm.GetManifestResourceNames() | Where-Object { $_ -like '*PawnIO_setup.exe' } | Select-Object -First 1
if (-not $res) { throw "This LibreHardwareMonitor.exe does not carry the PawnIO setup (older than 0.9.5?)" }
$setup = Join-Path ([IO.Path]::GetTempPath()) ("PawnIO_setup_" + [guid]::NewGuid() + ".exe")
$in = $asm.GetManifestResourceStream($res)
$out = [IO.File]::Create($setup)
try { $in.CopyTo($out) } finally { $out.Close(); $in.Close() }
try {
  $p = Start-Process -FilePath $setup -ArgumentList '-install' -Wait -PassThru
  if ($p.ExitCode -ne 0) { throw "PawnIO setup exited with code $($p.ExitCode)" }
} finally { Remove-Item $setup -Force -ErrorAction SilentlyContinue }
$pawn = ''
foreach ($k in 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO', 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO') {
  if (-not $pawn) { $pawn = [string](Get-ItemProperty $k -ErrorAction SilentlyContinue).DisplayVersion }
}
if (-not $pawn) { throw 'PawnIO setup finished but PawnIO is not registered as installed' }
Write-Host "PawnIO $pawn installed"
