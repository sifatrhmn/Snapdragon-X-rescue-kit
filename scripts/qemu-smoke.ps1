<#
.SYNOPSIS
  Fallback boot smoke test: boot the WinPE ARM64 ISO in QEMU on the x64 dev PC.

.DESCRIPTION
  Proves the image boots and the rescue GUI launches. It CANNOT prove the
  Snapdragon drivers (those need the real laptop). Prefer the Parallels VM
  (docs\parallels-test.md); use this only when the Mac is unavailable.

  Prerequisites (not installed by this script):
    - Portable QEMU for Windows (qemu.weilnetz.de) under tools\qemu\
    - EDK2 aarch64 UEFI firmware: edk2-aarch64-code.fd + edk2-arm-vars.fd
      (ship with QEMU under tools\qemu\share\)
  WinPE ARM64 needs -cpu max, ramfb (no virtio drivers in WinPE), and USB
  storage/kbd/tablet. Expect a boot of tens of minutes without acceleration.

.PARAMETER Iso
  Path to the WinPE ARM64 ISO (e.g. work\woa-rescue.iso).
#>
param([Parameter(Mandatory)][string]$Iso)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$qemu = Join-Path $root 'tools\qemu\qemu-system-aarch64.exe'
if (-not (Test-Path $qemu)) { throw "QEMU not found at $qemu - download portable QEMU to tools\qemu\ first." }
if (-not (Test-Path $Iso))  { throw "ISO not found: $Iso" }

$share = Join-Path $root 'tools\qemu\share'
$code  = Join-Path $share 'edk2-aarch64-code.fd'
$vars0 = Join-Path $share 'edk2-arm-vars.fd'
$vars  = Join-Path $root 'work\vars.fd'
if (-not (Test-Path $code))  { throw "missing firmware: $code" }
New-Item -ItemType Directory -Force -Path (Split-Path $vars) | Out-Null
Copy-Item $vars0 $vars -Force

$smoke = Join-Path $root 'work\smoke.vhdx'   # optional 2nd disk; create a 4 GiB SnapdragonLike VHDX if you want one listed

$args = @(
  '-M','virt','-cpu','max','-m','4G','-smp','4',
  '-drive',"if=pflash,format=raw,readonly=on,file=$code",
  '-drive',"if=pflash,format=raw,file=$vars",
  '-device','ramfb',
  '-device','qemu-xhci','-device','usb-kbd','-device','usb-tablet',
  '-drive',"if=none,id=cd,media=cdrom,file=$Iso",'-device','usb-storage,drive=cd',
  '-monitor','stdio'
)
if (Test-Path $smoke) {
  $args += @('-drive',"if=none,id=d1,file=$smoke",'-device','usb-storage,drive=d1')
}

Write-Host "Booting $Iso in QEMU (this is slow; the WinPE desktop + 'WoA Rescue Kit' window should appear)..."
& $qemu @args
