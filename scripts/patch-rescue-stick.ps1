<#
    Patch the rescue stick's boot image so it boots STRAIGHT to the menu
    (no "Type I / press Enter for Rescue Kit" prompt, no dead rescue.exe default).

    Start it by double-clicking patch-rescue-stick.cmd (which asks for admin).
    Run it from File Explorer, NOT from Claude Code: DISM must load the image's
    registry hives, and that is blocked for processes started by Claude Code.

    It edits only Windows\System32\startnet.cmd inside <stick>\sources\boot.wim.
    Everything else on the stick is untouched.
#>
$ErrorActionPreference = 'Stop'

function Fail($msg) { Write-Host "`nERROR: $msg" -ForegroundColor Red; Read-Host "Press Enter to close"; exit 1 }
function Ok($msg)   { Write-Host $msg -ForegroundColor Green }

# --- admin ---
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) { Fail "Not running as administrator. Double-click patch-rescue-stick.cmd instead." }

# --- the new start-up script (boots straight to the menu) ---
$startnet = @'
@echo off
rem WoA Rescue Kit start-up (Windows\System32\startnet.cmd): boot straight to the menu.
wpeinit
set WOA_USB=
set /a WOA_TRY=0
:findusb
for %%d in (C D E F G H I J K L M N O P Q R S T U V W Y Z) do if exist %%d:\woa-rescue.tag set WOA_USB=%%d:
if defined WOA_USB goto found
set /a WOA_TRY+=1
if %WOA_TRY% geq 15 goto found
echo Waiting for the rescue stick (%WOA_TRY% of 15)...
ping -n 3 127.0.0.1 >nul
goto findusb
:found
rem Clear the Snapdragon hidden/write-protect flag so tools can see the SSD (writes no data).
if defined WOA_USB if exist %WOA_USB%\woa-install.exe %WOA_USB%\woa-install.exe unhide --backup-dir %WOA_USB%\woa-backup >nul 2>&1
rem Capture a hardware/SSD log on every boot (silent; saved to the stick's \logs).
if defined WOA_USB if exist %WOA_USB%\collect-logs.cmd call "%WOA_USB%\collect-logs.cmd" >nul 2>&1
if defined WOA_USB if exist %WOA_USB%\woa-menu.cmd (
  call "%WOA_USB%\woa-menu.cmd"
  goto done
)
echo The rescue menu was not found on the stick; starting the Rescue Kit GUI.
set WOA_EXE=X:\Windows\System32\rescue.exe
if defined WOA_USB if exist %WOA_USB%\rescue.exe set WOA_EXE=%WOA_USB%\rescue.exe
"%WOA_EXE%"
:done
echo Rescue menu closed. Type  wpeutil reboot  to restart.
'@

# --- find the rescue stick (has woa-rescue.tag + sources\boot.wim) ---
$stick = $null
foreach ($v in Get-Volume | Where-Object DriveLetter) {
    $root = "$($v.DriveLetter):\"
    if ((Test-Path (Join-Path $root 'woa-rescue.tag')) -and (Test-Path (Join-Path $root 'sources\boot.wim'))) {
        $stick = $root; break
    }
}
if (-not $stick) { Fail "No rescue stick found (a drive with woa-rescue.tag and sources\boot.wim). Plug it in and run again." }
$wim = Join-Path $stick 'sources\boot.wim'
Ok "Rescue stick: $stick"
Write-Host "boot.wim:    $wim"

# --- DISM hive self-test (DISM cannot service the image without it) ---
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$hive  = Join-Path $env:TEMP "woa-hive-$stamp.hiv"
& reg.exe save HKCU\Environment $hive /y | Out-Null
$load = & cmd.exe /c "reg load HKLM\WOAPATCHTEST `"$hive`" 2>&1"
if ($load -notmatch 'success') {
    Remove-Item $hive -Force -ErrorAction SilentlyContinue
    Fail ("Registry hive loading fails on this PC, so DISM cannot edit the image.`r`n" +
          "Make sure you double-clicked this from File Explorer (not Claude Code), and that`r`n" +
          "Controlled Folder Access is off. If it still fails, use a different Windows 11 PC.")
}
& reg.exe unload HKLM\WOAPATCHTEST | Out-Null
Remove-Item $hive -Force -ErrorAction SilentlyContinue
Ok "DISM hive self-test: OK"

# --- mount, replace startnet.cmd, commit ---
$mount = Join-Path $env:TEMP "woa-mount-$stamp"
New-Item -ItemType Directory -Force $mount | Out-Null
Write-Host "`nMounting the boot image (this takes a minute)..."
& dism.exe /Mount-Wim /WimFile:"$wim" /Index:1 /MountDir:"$mount"
if ($LASTEXITCODE -ne 0) { Remove-Item $mount -Recurse -Force -ErrorAction SilentlyContinue; Fail "DISM could not mount the image (exit $LASTEXITCODE)." }

try {
    $target = Join-Path $mount 'Windows\System32\startnet.cmd'
    # Write CRLF, ASCII - a .cmd must be CRLF or cmd.exe mis-parses it.
    $text = ($startnet -replace "`r`n","`n") -replace "`n","`r`n"
    [IO.File]::WriteAllText($target, $text, (New-Object System.Text.UTF8Encoding($false)))
    Ok "Replaced Windows\System32\startnet.cmd"
} catch {
    & dism.exe /Unmount-Wim /MountDir:"$mount" /Discard | Out-Null
    Remove-Item $mount -Recurse -Force -ErrorAction SilentlyContinue
    Fail "Could not write the new startnet.cmd: $_"
}

Write-Host "Saving the image (commit)..."
& dism.exe /Unmount-Wim /MountDir:"$mount" /Commit
if ($LASTEXITCODE -ne 0) { Fail "DISM could not save the image (exit $LASTEXITCODE). The stick may be unchanged." }
Remove-Item $mount -Recurse -Force -ErrorAction SilentlyContinue

Write-Host ""
Ok "=============================================================="
Ok " DONE. The stick now boots straight to the menu - no Enter prompt."
Ok "=============================================================="
Write-Host "Eject the stick and boot the PZ13; the Install / Back up / Restore menu"
Write-Host "opens by itself."
Read-Host "Press Enter to close"
