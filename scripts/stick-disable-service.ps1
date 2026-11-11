<#
.SYNOPSIS
  Disables one driver service (default: spaceport, Storage Spaces) in the WinPE image on
  the rescue stick, as a diagnostic test.

.DESCRIPTION
  Start it with scripts\stick-disable-service.cmd (double-click in File Explorer).
  Run it yourself, NOT from Claude Code: editing the image's registry loads a hive, and
  hive loads fail for processes started by Claude Code (anthropics/claude-code#105880).

  Steps: administrator check -> registry-hive self-test -> find the stick's boot partition
  (a volume labelled WOARESCUE with \woa-rescue.tag and \sources\boot.wim) -> copy its
  boot.wim to work\stick3 (a dated backup is kept) -> mount -> load SYSTEM hive -> set
  Services\<name>\Start = 4 in every ControlSet -> unload -> commit -> copy back.
  Result: work\logs\stick-disable-service-result.txt.
#>
param([string]$Service = 'spaceport', [int]$Start = 4)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$logs = Join-Path $root 'work\logs'
New-Item -ItemType Directory -Force $logs | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$summary = Join-Path $logs 'stick-disable-service-result.txt'
Start-Transcript -Path (Join-Path $logs "stick-disable-service-$stamp.txt") | Out-Null
$work = Join-Path $root 'work\stick3'
$mount = Join-Path $work 'mount'
$wim = Join-Path $work 'boot.wim'
$hiveKey = 'HKLM\WOAPESYSTEM'
$script:mounted = $false
$script:hive = $false

function Result([string]$status, [string]$detail) {
    if ($script:hive) { & reg.exe unload $hiveKey | Out-Null }
    if ($script:mounted) { & dism.exe /English /Unmount-Wim /MountDir:"$mount" /Discard | Out-Null }
    $text = "RESULT: $status`r`nwhen:   $(Get-Date -Format o)`r`n$detail"
    Set-Content -Path $summary -Value $text -Encoding utf8
    Write-Host ''
    Write-Host $text -ForegroundColor $(if ($status -eq 'SUCCESS') { 'Green' } else { 'Red' })
    Stop-Transcript | Out-Null
    exit $(if ($status -eq 'SUCCESS') { 0 } else { 1 })
}

$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) { Result 'FAILED' 'Not running as administrator. Start scripts\stick-disable-service.cmd instead.' }

$test = Join-Path $env:TEMP "woa-hive-$stamp.hiv"
& reg.exe save HKCU\Environment $test /y | Out-Null
$load = & cmd.exe /c "reg load HKLM\WOAHIVESELFTEST `"$test`" 2>&1"
if ($load -notmatch 'success') {
    Remove-Item $test -Force -ErrorAction SilentlyContinue
    Result 'FAILED' "Registry hive loading fails here: $load`r`nDouble-click scripts\stick-disable-service.cmd in File Explorer instead of starting it from Claude Code."
}
& reg.exe unload HKLM\WOAHIVESELFTEST | Out-Null
Remove-Item $test -Force -ErrorAction SilentlyContinue

$stick = Get-Volume | Where-Object { $_.DriveLetter -and $_.FileSystemLabel -eq 'WOARESCUE' } |
    ForEach-Object { "$($_.DriveLetter):" } |
    Where-Object { (Test-Path "$_\woa-rescue.tag") -and (Test-Path "$_\sources\boot.wim") } | Select-Object -First 1
if (-not $stick) { Result 'FAILED' 'Rescue stick not found (a WOARESCUE volume with \woa-rescue.tag and \sources\boot.wim). Plug in the SanDisk and run this again.' }
Write-Host "Rescue stick: $stick" -ForegroundColor Cyan

New-Item -ItemType Directory -Force $mount | Out-Null
if (Get-ChildItem $mount -Force -ErrorAction SilentlyContinue) {
    & dism.exe /English /Unmount-Wim /MountDir:"$mount" /Discard | Out-Null
    & dism.exe /English /Cleanup-Wim | Out-Null
}
Copy-Item "$stick\sources\boot.wim" $wim -Force
Copy-Item $wim (Join-Path $work "boot.wim.$stamp.bak") -Force

& dism.exe /English /Mount-Wim /WimFile:"$wim" /Index:1 /MountDir:"$mount"
if ($LASTEXITCODE -ne 0) { Result 'FAILED' "DISM could not mount $wim (exit $LASTEXITCODE)." }
$script:mounted = $true
& reg.exe load $hiveKey "$mount\Windows\System32\config\SYSTEM" | Out-Null
if ($LASTEXITCODE -ne 0) { Result 'FAILED' 'Could not load the image SYSTEM hive.' }
$script:hive = $true
$sets = (& reg.exe query $hiveKey) | Where-Object { $_ -match 'ControlSet\d{3}$' }
$changed = @()
foreach ($cs in $sets) {
    $key = "$cs\Services\$Service"
    & reg.exe query $key /v Start 2>$null | Out-Null
    if ($LASTEXITCODE -ne 0) { continue }
    & reg.exe add $key /v Start /t REG_DWORD /d $Start /f | Out-Null
    if ($LASTEXITCODE -ne 0) { Result 'FAILED' "Could not set $key\Start." }
    $changed += ($cs -split '\\')[-1]
}
if ($changed.Count -eq 0) { Result 'FAILED' "Service '$Service' not found in the image registry." }
[gc]::Collect()
& reg.exe unload $hiveKey | Out-Null
if ($LASTEXITCODE -ne 0) { Result 'FAILED' 'Could not unload the image SYSTEM hive (close any Registry Editor windows and try again).' }
$script:hive = $false
& dism.exe /English /Unmount-Wim /MountDir:"$mount" /Commit
if ($LASTEXITCODE -ne 0) { Result 'FAILED' "DISM could not commit the image (exit $LASTEXITCODE)." }
$script:mounted = $false
Copy-Item $wim "$stick\sources\boot.wim" -Force
Write-VolumeCache $stick.TrimEnd(':')
if ((Get-FileHash "$stick\sources\boot.wim").Hash -ne (Get-FileHash $wim).Hash) { Result 'FAILED' 'The boot.wim on the stick does not match. Run this again.' }
$word = switch ($Start) { 0 { 'boot-start (enabled)' } 4 { 'disabled' } default { "Start=$Start" } }
Result 'SUCCESS' "stick:   $stick`r`nservice: $Service Start=$Start ($word) in $($changed -join ', ')`r`nbackup:  work\stick3\boot.wim.$stamp.bak`r`nYou can now unplug the stick and boot the tablet from it."
