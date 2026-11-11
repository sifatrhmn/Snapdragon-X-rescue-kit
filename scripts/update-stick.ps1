<#
.SYNOPSIS
  Adds the drivers in work\drivers-extra to the WinPE image on the rescue USB stick.

.DESCRIPTION
  Start it with scripts\update-stick.cmd (double-click it in File Explorer); that asks for
  administrator rights and then runs this script.

  Run it yourself, NOT from Claude Code: DISM /Add-Driver must load the image's registry
  hives, and hive loads fail for processes started by Claude Code
  (anthropics/claude-code#105880).

  Steps: administrator check -> Controlled Folder Access off -> registry-hive self-test ->
  find the stick (a USB volume with \woa-rescue.tag and \sources\boot.wim) -> copy its
  boot.wim to work\stick (a dated backup is kept) -> mount -> /Add-Driver /Recurse ->
  check the drivers are in the image -> commit -> copy back to the stick and to
  work\pe\media. The stick is not reformatted. A summary is written to
  work\logs\update-stick-result.txt.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$logs = Join-Path $root 'work\logs'
New-Item -ItemType Directory -Force $logs | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$summary = Join-Path $logs 'update-stick-result.txt'
Start-Transcript -Path (Join-Path $logs "update-stick-$stamp.txt") | Out-Null

$work = Join-Path $root 'work\stick'
$mount = Join-Path $work 'mount'
$wim = Join-Path $work 'boot.wim'
$extra = Join-Path $root 'work\drivers-extra'
$mounted = $false

function Result([string]$status, [string]$detail) {
    if ($script:mounted) {
        Write-Host 'Discarding the mounted image...'
        & dism.exe /English /Unmount-Wim /MountDir:"$mount" /Discard | Out-Null
    }
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
if (-not $admin) { Result 'FAILED' 'Not running as administrator. Start scripts\update-stick.cmd instead.' }

# 2. Controlled Folder Access blocks dism.exe.
if ((Get-MpPreference).EnableControlledFolderAccess -ne 0) {
    Result 'FAILED' 'Controlled Folder Access is on. Turn it off (Windows Security > Ransomware protection) and run this again.'
}

# 3. Registry-hive self-test: /Add-Driver cannot work without it.
$hive = Join-Path $env:TEMP "woa-hive-$stamp.hiv"
& reg.exe save HKCU\Environment $hive /y | Out-Null
$load = & cmd.exe /c "reg load HKLM\WOAHIVESELFTEST `"$hive`" 2>&1"
if ($load -notmatch 'success') {
    Remove-Item $hive -Force -ErrorAction SilentlyContinue
    Result 'FAILED' ("Registry hive loading fails here: $load`r`n" +
        'If you started this from Claude Code, double-click scripts\update-stick.cmd in File Explorer instead.')
}
& reg.exe unload HKLM\WOAHIVESELFTEST | Out-Null
Remove-Item $hive -Force -ErrorAction SilentlyContinue
Write-Host 'Registry hive self-test: OK' -ForegroundColor Green

# 4. Drivers to add.
$infs = @(Get-ChildItem $extra -Recurse -Filter *.inf -ErrorAction SilentlyContinue)
if ($infs.Count -eq 0) { Result 'FAILED' "No drivers found in $extra." }
Write-Host "Drivers to add: $(($infs | ForEach-Object Name) -join ', ')"

# 5. Find the rescue stick.
$stick = $null
foreach ($v in Get-Volume | Where-Object { $_.DriveLetter -and $_.DriveType -eq 'Removable' }) {
    $l = "$($v.DriveLetter):"
    if ((Test-Path "$l\woa-rescue.tag") -and (Test-Path "$l\sources\boot.wim")) { $stick = $l; break }
}
if (-not $stick) { Result 'FAILED' 'Rescue USB stick not found (a removable drive with \woa-rescue.tag and \sources\boot.wim). Plug it in and run this again.' }
Write-Host "Rescue stick: $stick" -ForegroundColor Cyan

# 6. Copy the image off the stick, keeping a dated backup.
New-Item -ItemType Directory -Force $mount | Out-Null
if (Get-ChildItem $mount -Force -ErrorAction SilentlyContinue) {
    Write-Host 'Discarding a stale mount...'
    & dism.exe /English /Unmount-Wim /MountDir:"$mount" /Discard | Out-Null
    & dism.exe /English /Cleanup-Wim | Out-Null
}
Write-Host 'Copying boot.wim from the stick...'
Copy-Item "$stick\sources\boot.wim" $wim -Force
Copy-Item $wim (Join-Path $work "boot.wim.$stamp.bak") -Force

# 7. Mount, add drivers, check, commit.
& dism.exe /English /Mount-Wim /WimFile:"$wim" /Index:1 /MountDir:"$mount"
if ($LASTEXITCODE -ne 0) { Result 'FAILED' "DISM could not mount $wim (exit $LASTEXITCODE)." }
$script:mounted = $true
& dism.exe /English /Image:"$mount" /Add-Driver /Driver:"$extra" /Recurse
if ($LASTEXITCODE -ne 0) { Result 'FAILED' "DISM /Add-Driver failed (exit $LASTEXITCODE). See C:\Windows\Logs\DISM\dism.log." }
$list = (& dism.exe /English /Image:"$mount" /Get-Drivers /Format:Table) -join "`n"
$missing = @($infs | Where-Object { $list -notmatch [regex]::Escape($_.Name) })
if ($missing.Count -gt 0) { Result 'FAILED' "Driver(s) not found in the image after adding: $(($missing | ForEach-Object Name) -join ', ')" }
Write-Host 'Committing the image (about a minute)...'
& dism.exe /English /Unmount-Wim /MountDir:"$mount" /Commit
if ($LASTEXITCODE -ne 0) { Result 'FAILED' "DISM could not commit the image (exit $LASTEXITCODE)." }
$script:mounted = $false

# 8. Put it back on the stick and in the build output.
Write-Host 'Copying the updated boot.wim to the stick...'
Copy-Item $wim "$stick\sources\boot.wim" -Force
Write-VolumeCache $stick.TrimEnd(':')
Copy-Item $wim (Join-Path $root 'work\pe\media\sources\boot.wim') -Force
$same = (Get-FileHash "$stick\sources\boot.wim").Hash -eq (Get-FileHash $wim).Hash
if (-not $same) { Result 'FAILED' 'The boot.wim on the stick does not match the updated image. Run this again.' }
$added = ($infs | ForEach-Object Name) -join ', '
Result 'SUCCESS' "stick:  $stick`r`nadded:  $added`r`nimage:  $wim`r`nbackup: boot.wim.$stamp.bak`r`nYou can now unplug the stick and boot the tablet from it."
