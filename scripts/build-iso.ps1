<#
.SYNOPSIS
  Section C of docs\parallels-test.md in one go: build the WinPE ARM64 image and the ISO.

.DESCRIPTION
  Start it with scripts\build-iso.cmd (double-click it in File Explorer); that asks for
  administrator rights and then runs this script.

  Run it yourself, NOT from Claude Code: DISM must load the image's registry hives, and
  hive loads fail for processes started by Claude Code (anthropics/claude-code#105880).

  Steps: administrator check -> Controlled Folder Access must be off -> registry-hive
  self-test -> binaries present -> remove a stale work\pe -> woa-builder build ->
  woa-builder iso -> verify the ISO contents. A summary is written to
  work\logs\build-iso-result.txt so the result can be checked afterwards.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$logs = Join-Path $root 'work\logs'
New-Item -ItemType Directory -Force $logs | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$summary = Join-Path $logs 'build-iso-result.txt'
Start-Transcript -Path (Join-Path $logs "build-iso-$stamp.txt") | Out-Null

function Result([string]$status, [string]$detail) {
    $text = "RESULT: $status`r`nwhen:   $(Get-Date -Format o)`r`nhost:   $env:COMPUTERNAME`r`n$detail"
    Set-Content -Path $summary -Value $text -Encoding utf8
    Write-Host ''
    Write-Host $text -ForegroundColor $(if ($status -eq 'SUCCESS') { 'Green' } else { 'Red' })
    Stop-Transcript | Out-Null
    exit $(if ($status -eq 'SUCCESS') { 0 } else { 1 })
}

# 1. Administrator
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) { Result 'FAILED' 'Not running as administrator. Start scripts\build-iso.cmd instead.' }

# 2. Controlled Folder Access blocks dism.exe from writing disk sectors.
$cfa = (Get-MpPreference).EnableControlledFolderAccess
if ($cfa -ne 0) {
    Result 'FAILED' ("Controlled Folder Access is on ($cfa). Turn it off (Windows Security > Ransomware protection," +
        " or: Set-MpPreference -EnableControlledFolderAccess Disabled) and run this again.")
}

# 3. Registry-hive self-test: DISM cannot service the image without it.
$hive = Join-Path $env:TEMP "woa-hive-$stamp.hiv"
& reg.exe save HKCU\Environment $hive /y | Out-Null
$load = & cmd.exe /c "reg load HKLM\WOAHIVESELFTEST `"$hive`" 2>&1"
if ($load -notmatch 'success') {
    Remove-Item $hive -Force -ErrorAction SilentlyContinue
    Result 'FAILED' ("Registry hive loading fails here too: $load`r`n" +
        "If you started this from Claude Code, double-click scripts\build-iso.cmd in File Explorer instead.`r`n" +
        "If it fails from File Explorer too, DISM cannot service the WinPE image on this machine:" +
        " build on a Windows 11 24H2/25H2 machine instead.")
}
& reg.exe unload HKLM\WOAHIVESELFTEST | Out-Null
Remove-Item $hive -Force -ErrorAction SilentlyContinue
Write-Host 'Registry hive self-test: OK' -ForegroundColor Green

# 4. Binaries
$builder = Join-Path $root 'target\release\woa-builder.exe'
$rescue = Join-Path $root 'target\aarch64-pc-windows-msvc\release\rescue.exe'
$drivers = Join-Path $root 'work\drivers'
foreach ($p in $builder, $rescue) {
    if (-not (Test-Path $p)) { Result 'FAILED' "Missing $p. Build it first (cargo build)." }
}
if (-not (Test-Path (Join-Path $drivers 'drivers.json'))) {
    Result 'FAILED' "Missing $drivers\drivers.json. Run: woa-builder drivers import --from <Snapdragon Qcom Drivers> --out work\drivers"
}

# 5. The builder refuses to reuse a work directory.
$pe = Join-Path $root 'work\pe'
if (Test-Path $pe) { Write-Host "Removing previous $pe"; Remove-Item $pe -Recurse -Force }

# 6. Build the image.
Write-Host 'Building the WinPE ARM64 image (several minutes)...' -ForegroundColor Cyan
& $builder build --workdir $pe --drivers $drivers --rescue $rescue
if ($LASTEXITCODE -ne 0) { Result 'FAILED' "woa-builder build failed (exit $LASTEXITCODE). See the newest work\logs\build-*.log." }

# 7. Make the ISO.
$iso = Join-Path $root 'work\woa-rescue.iso'
if (Test-Path $iso) { Remove-Item $iso -Force }
Write-Host 'Creating the ISO...' -ForegroundColor Cyan
& $builder iso --workdir $pe --out $iso
if ($LASTEXITCODE -ne 0 -or -not (Test-Path $iso)) { Result 'FAILED' "woa-builder iso failed (exit $LASTEXITCODE)." }

# 8. Verify the ISO contents.
$img = Mount-DiskImage -ImagePath $iso -Access ReadOnly -PassThru
try {
    $letter = ($img | Get-Volume).DriveLetter
    $loader = Test-Path "${letter}:\EFI\Boot\bootaa64.efi"
    $wim = Test-Path "${letter}:\sources\boot.wim"
    $issuer = if ($loader) { (Get-AuthenticodeSignature "${letter}:\EFI\Boot\bootaa64.efi").SignerCertificate.Issuer } else { '' }
} finally { Dismount-DiskImage -ImagePath $iso | Out-Null }
if (-not ($loader -and $wim)) {
    Result 'FAILED' "ISO is incomplete: bootaa64.efi=$loader, sources\boot.wim=$wim"
}
# Snapdragon X firmware (2024+) trusts the 2023 CA; the 2011 CA is being revoked.
if ($issuer -notmatch 'Windows UEFI CA 2023') {
    Result 'FAILED' "bootaa64.efi is signed by '$issuer', not 'Windows UEFI CA 2023'."
}
$size = '{0:N0} MB' -f ((Get-Item $iso).Length / 1MB)
$sha = (Get-FileHash $iso -Algorithm SHA256).Hash
Result 'SUCCESS' "iso:    $iso`r`nsize:   $size`r`nsha256: $sha`r`nchecks: EFI\Boot\bootaa64.efi (Windows UEFI CA 2023) and sources\boot.wim present"
