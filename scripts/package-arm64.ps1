<#
.SYNOPSIS
  Build the release ARM64 binaries and stage a handoff folder for the
  Parallels Windows-on-ARM VM (or a real Snapdragon laptop).

.DESCRIPTION
  Produces work\handoff\ containing:
    - woa-builder.exe  (ARM64 host builder)
    - rescue.exe       (ARM64 WinPE rescue GUI)
    - woa-rescue.iso   (only if it has already been built on an ADK machine)
    - SHA256SUMS.txt
    - parallels-test.md (the test checklist, copied from docs\)
  Copy work\handoff\ AND the "Snapdragon Qcom Drivers" folder to the VM,
  then follow parallels-test.md.

  Run from the repo root on any machine with the Rust aarch64 toolchain:
      powershell -ExecutionPolicy Bypass -File scripts\package-arm64.ps1
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
if (Get-Command cargo -ErrorAction SilentlyContinue) { } else { $env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path" }

Write-Host 'Building release ARM64 binaries...'
cargo build --workspace --release --target aarch64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw "ARM64 build failed ($LASTEXITCODE)" }

$bin = Join-Path $root 'target\aarch64-pc-windows-msvc\release'
$out = Join-Path $root 'work\handoff'
New-Item -ItemType Directory -Force -Path $out | Out-Null
Get-ChildItem $out -File -ErrorAction SilentlyContinue | Remove-Item -Force

foreach ($exe in 'woa-builder.exe','rescue.exe') {
  $src = Join-Path $bin $exe
  if (-not (Test-Path $src)) { throw "missing $src" }
  Copy-Item $src $out -Force
}

$iso = Join-Path $root 'work\woa-rescue.iso'
if (Test-Path $iso) { Copy-Item $iso $out -Force; Write-Host "included $(Split-Path $iso -Leaf)" }
else { Write-Host 'No ISO yet (build it on the VM with woa-builder) - staging binaries only.' }

$doc = Join-Path $root 'docs\parallels-test.md'
if (Test-Path $doc) { Copy-Item $doc $out -Force }

# Confirm the staged binaries are really ARM64 (PE machine 0xAA64).
function Get-PeMachine([string]$p) {
  $fs = [IO.File]::OpenRead($p); try {
    $br = New-Object IO.BinaryReader($fs)
    $fs.Position = 0x3C; $off = $br.ReadInt32(); $fs.Position = $off + 4
    '0x{0:X4}' -f $br.ReadUInt16()
  } finally { $fs.Dispose() }
}
foreach ($exe in 'woa-builder.exe','rescue.exe') {
  $m = Get-PeMachine (Join-Path $out $exe)
  Write-Host ("{0}: machine {1}" -f $exe, $m)
  if ($m -ne '0xAA64') { throw "$exe is $m, not ARM64 (0xAA64)" }
}

# Checksums.
$sums = Join-Path $out 'SHA256SUMS.txt'
Get-ChildItem $out -File | Where-Object { $_.Name -ne 'SHA256SUMS.txt' } | ForEach-Object {
  "{0}  {1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash, $_.Name
} | Set-Content $sums -Encoding ascii

Write-Host ''
Write-Host "Handoff staged at: $out"
Get-ChildItem $out | Select-Object Name, Length | Format-Table -AutoSize
Write-Host 'Next: copy work\handoff\ and the Snapdragon Qcom Drivers folder to the VM, then follow parallels-test.md.'
