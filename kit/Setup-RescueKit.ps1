<#
    WoA Rescue Kit - USB builder
    ============================
    Makes the two USB sticks that fix an ASUS ProArt PZ13 (Snapdragon) whose
    internal SSD shows "no disk" in Windows Setup, and then installs Windows
    on it.

    Just right-click this file and choose "Run with PowerShell".
    It will ask you, step by step, which USB stick to use. Nothing is touched
    until you confirm. Read the on-screen questions carefully - the stick you
    pick is completely erased.

    Needs: two USB sticks.
      - Stick 1 (the fix tool): any size, even 1 GB.
      - Stick 2 (the installer): 16 GB or larger.
#>

$ErrorActionPreference = 'Stop'
$KitRoot = $PSScriptRoot

# ---- re-launch as Administrator if needed -------------------------------
$admin = ([Security.Principal.WindowsPrincipal] `
    [Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) {
    Write-Host "Asking for Administrator rights..." -ForegroundColor Yellow
    Start-Process powershell.exe `
        "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`"" -Verb RunAs
    return
}

function Pause-Exit($code = 0) {
    Write-Host ""
    Read-Host "Press Enter to close this window"
    exit $code
}

function Banner($text) {
    Write-Host ""
    Write-Host ("=" * 70) -ForegroundColor Cyan
    Write-Host "  $text" -ForegroundColor Cyan
    Write-Host ("=" * 70) -ForegroundColor Cyan
}

# ---- sanity: are the payload folders present? ---------------------------
$gptfixEfi = Join-Path $KitRoot "1-gptfix-stick\EFI\Boot\bootaa64.efi"
$winpeBoot = Join-Path $KitRoot "2-winpe-stick\boot"
$winpeData = Join-Path $KitRoot "2-winpe-stick\data"
foreach ($p in @($gptfixEfi, $winpeBoot, $winpeData)) {
    if (-not (Test-Path $p)) {
        Write-Host "ERROR: missing kit file/folder:" -ForegroundColor Red
        Write-Host "  $p"
        Write-Host "Run this script from inside the WoA-Rescue-Kit folder."
        Pause-Exit 1
    }
}

Banner "WoA Rescue Kit - USB builder"
Write-Host @"
This will build the two rescue USB sticks.

  Stick 1  'gptfix'   - the UEFI tool that un-hides the PZ13's SSD.
  Stick 2  'WinPE'    - boots a mini-Windows that installs Windows on the SSD.

WARNING: each stick you pick is ERASED completely. Unplug every USB drive
you want to keep, then plug in only the stick(s) you will use for the kit.
"@ -ForegroundColor Gray

# ---- helper: let the user pick a USB disk safely ------------------------
function Select-UsbDisk($purpose, $minBytes) {
    while ($true) {
        $disks = Get-Disk | Where-Object { $_.BusType -eq 'USB' } |
            Sort-Object Number
        Banner "Pick the USB stick for: $purpose"
        if (-not $disks) {
            Write-Host "No USB disks found. Plug the stick in and..." -ForegroundColor Yellow
            if ((Read-Host "type R then Enter to re-scan, or Q to quit") -match '^[Qq]') { Pause-Exit 0 }
            continue
        }
        $disks | ForEach-Object {
            $gb = [math]::Round($_.Size / 1GB, 1)
            Write-Host ("  Disk {0}  {1,-28} {2,7} GB" -f $_.Number, $_.FriendlyName, $gb)
        }
        Write-Host ""
        $ans = Read-Host "Type the Disk number to USE (this ERASES it), or R to re-scan, or Q to quit"
        if ($ans -match '^[Qq]') { Pause-Exit 0 }
        if ($ans -match '^[Rr]') { continue }
        $num = 0
        if (-not [int]::TryParse($ans, [ref]$num)) { Write-Host "Not a number." -ForegroundColor Red; continue }
        $d = $disks | Where-Object Number -eq $num
        if (-not $d) { Write-Host "No USB disk with that number." -ForegroundColor Red; continue }
        if ($d.Size -lt $minBytes) {
            Write-Host ("That stick is too small. Need at least {0} GB." -f [math]::Round($minBytes/1GB,0)) -ForegroundColor Red
            continue
        }
        $gb = [math]::Round($d.Size / 1GB, 1)
        Write-Host ""
        Write-Host ("You chose Disk {0}: {1} ({2} GB)." -f $d.Number, $d.FriendlyName, $gb) -ForegroundColor Yellow
        Write-Host "EVERYTHING on it will be permanently erased." -ForegroundColor Red
        $c = Read-Host "Type  ERASE  (in capitals) to confirm, or anything else to go back"
        if ($c -ceq 'ERASE') { return $d }
    }
}

# ======================  STICK 1 : gptfix  ===============================
$d1 = Select-UsbDisk "Stick 1 - gptfix (the fix tool, any size)" (0.2GB)
Write-Host "`nPreparing Stick 1..." -ForegroundColor Green
Clear-Disk -Number $d1.Number -RemoveData -RemoveOEM -Confirm:$false
Initialize-Disk -Number $d1.Number -PartitionStyle GPT -Confirm:$false
$p1 = New-Partition -DiskNumber $d1.Number -UseMaximumSize -AssignDriveLetter
$v1 = Format-Volume -Partition $p1 -FileSystem FAT32 -NewFileSystemLabel "GPTFIX" -Confirm:$false
$drv1 = "$($v1.DriveLetter):"
New-Item -ItemType Directory -Force -Path "$drv1\EFI\Boot" | Out-Null
Copy-Item $gptfixEfi "$drv1\EFI\Boot\bootaa64.efi" -Force
Copy-Item (Join-Path $KitRoot "README.html") "$drv1\README.html" -Force -ErrorAction SilentlyContinue
Copy-Item (Join-Path $KitRoot "PZ13-Complete-Guide.html") "$drv1\PZ13-Complete-Guide.html" -Force -ErrorAction SilentlyContinue
Write-Host "Stick 1 (gptfix) is ready on $drv1." -ForegroundColor Green

# ======================  STICK 2 : WinPE installer  ======================
$d2 = Select-UsbDisk "Stick 2 - WinPE installer (16 GB or larger)" (14GB)
Write-Host "`nPreparing Stick 2 (this copies ~9 GB and takes a few minutes)..." -ForegroundColor Green
Clear-Disk -Number $d2.Number -RemoveData -RemoveOEM -Confirm:$false
Initialize-Disk -Number $d2.Number -PartitionStyle GPT -Confirm:$false
# Partition 1: 2 GB FAT32 boot (WinPE boots from here)
$boot = New-Partition -DiskNumber $d2.Number -Size 2GB -AssignDriveLetter
$vb = Format-Volume -Partition $boot -FileSystem FAT32 -NewFileSystemLabel "WOARESCUE" -Confirm:$false
$bDrv = "$($vb.DriveLetter):"
# Partition 2: rest NTFS data (holds the big Windows image)
$data = New-Partition -DiskNumber $d2.Number -UseMaximumSize -AssignDriveLetter
$vd = Format-Volume -Partition $data -FileSystem NTFS -NewFileSystemLabel "WOADATA" -Confirm:$false
$dDrv = "$($vd.DriveLetter):"

Write-Host "Copying boot files to $bDrv ..." -ForegroundColor Green
robocopy $winpeBoot "$bDrv\" /E /NFL /NDL /NP /R:1 /W:1 | Out-Null
Write-Host "Copying Windows image + drivers to $dDrv ... (the big one)" -ForegroundColor Green
robocopy "$winpeData\win"     "$dDrv\win"     /E /NFL /NDL /NP /R:1 /W:1 | Out-Null
robocopy "$winpeData\drivers" "$dDrv\drivers" /E /NFL /NDL /NP /R:1 /W:1 | Out-Null
Copy-Item (Join-Path $KitRoot "README.html") "$bDrv\README.html" -Force -ErrorAction SilentlyContinue
Copy-Item (Join-Path $KitRoot "PZ13-Complete-Guide.html") "$bDrv\PZ13-Complete-Guide.html" -Force -ErrorAction SilentlyContinue
Write-Host "Stick 2 (WinPE installer) is ready: boot=$bDrv data=$dDrv." -ForegroundColor Green

Banner "Both sticks are ready!"
Write-Host @"
Now, on the ASUS PZ13 tablet:

  1. Turn the tablet off. Tap F2 at the ASUS logo to open BIOS/Setup.
  2. Set  Secure Boot = OFF.  Save and exit.
  3. Plug in STICK 1 (gptfix) only. Boot from it (F2 > boot menu > the USB).
     (Only needed if the SSD shows as empty/hidden. It lists the partitions;
      type  YES , Enter; when it says "DONE: verified", press Enter to restart.)
  4. Tap F2 again, set  Secure Boot = ON,  save and exit.
  5. Unplug Stick 1. Plug in STICK 2 (WinPE). Boot from it.
  6. The rescue menu opens by itself (an older stick first asks you to
     type  I  and press Enter):
        [1] Install Windows (clean)
        [2] Back up the internal disk  -> to another drive you plug in
        [3] Restore the internal disk  -> from a backup on another drive
        [4] Rescue tools   [5] Command prompt   [6] Restart   [7] Shut down

For a backup, also bring a second portable drive (exFAT/NTFS) with room.
Quick start: README.html. Full guide: PZ13-Complete-Guide.html (both are
in this folder and on each stick).
"@ -ForegroundColor Gray
Pause-Exit 0
