# Installs Windows ADK 26100.2454 Deployment Tools + the WinPE add-on WITHOUT adksetup.exe.
#
# Why: Microsoft re-signed the ADK payloads on its CDN (Mar 2026) but the bootstrapper
# manifests still carry the old SHA-1 hashes, so adksetup fails with 0x80091007.
# This script reads the bundle manifest, downloads each MSI/CAB itself, accepts a file only
# if it matches the manifest hash OR has a valid "Microsoft Corporation" Authenticode
# signature, then installs the MSIs with msiexec.
#
# Run from an elevated PowerShell. Needs winget.
#   powershell -ExecutionPolicy Bypass -File scripts\install-adk.ps1
$ErrorActionPreference = 'Stop'
$work = Join-Path $env:TEMP 'woa-adk'
$kitsRoot = "${env:ProgramFiles(x86)}\Windows Kits\10\"
$version = '10.1.26100.2454'
$bundles = @(
    @{ Key = 'adk'; Id = 'Microsoft.WindowsADK'
       Base = 'https://download.microsoft.com/download/8e0c0f5a-abb5-4358-a51b-168eb40b1590/adk/'
       Options = @('OptionId.KitsConfiguration', 'OptionId.DeploymentTools') },
    @{ Key = 'pe'; Id = 'Microsoft.WindowsADK.WinPEAddon'
       Base = 'https://download.microsoft.com/download/a4a79e7a-f085-41c4-aebf-2538fd000790/adkwinpeaddons/'
       Options = @('OptionId.WindowsPreinstallationEnvironment') }
)

function Get-BurnManifest([string]$exe, [string]$out) {
    # The first cabinet in a WiX Burn bundle is the UX container; file "0" is the manifest.
    $b = [IO.File]::ReadAllBytes($exe)
    for ($i = 0; $i -lt $b.Length - 16; $i++) {
        if ($b[$i] -eq 0x4D -and $b[$i+1] -eq 0x53 -and $b[$i+2] -eq 0x43 -and $b[$i+3] -eq 0x46 -and $b[$i+4] -eq 0) {
            $len = [BitConverter]::ToUInt32($b, $i + 8)
            New-Item -ItemType Directory -Force $out | Out-Null
            [IO.File]::WriteAllBytes("$out\ux.cab", $b[$i..($i + $len - 1)])
            expand.exe "$out\ux.cab" -F:0 $out | Out-Null
            return [xml](Get-Content "$out\0" -Raw)
        }
    }
    throw "no manifest found in $exe"
}

function Get-KitsOption($pkg) { ($pkg.MsiProperty | Where-Object Id -eq KITSOPTION).Value }

# A freshly downloaded file can stay locked for a few seconds while antivirus scans it
# ("being used by another process"), so retry the read instead of failing the install.
function Invoke-WithRetry([scriptblock]$Action) {
    for ($i = 1; ; $i++) {
        try { return & $Action }
        catch { if ($i -ge 15) { throw }; Start-Sleep -Seconds 2 }
    }
}

foreach ($bun in $bundles) {
    $dir = Join-Path $work $bun.Key
    New-Item -ItemType Directory -Force $dir | Out-Null
    winget download --id $bun.Id -v $version -d "$dir\setup" --accept-source-agreements --accept-package-agreements | Out-Null
    $exe = Get-ChildItem "$dir\setup" -Filter *.exe | Select-Object -First 1
    $m = Get-BurnManifest $exe.FullName "$dir\ux"
    $payloads = @{}; $m.BurnManifest.Payload | ForEach-Object { $payloads[$_.Id] = $_ }
    # Kits configuration first so KITSROOT is registered before the feature packages.
    $pkgs = $m.BurnManifest.Chain.MsiPackage | Where-Object { (Get-KitsOption $_) -in $bun.Options } |
        Sort-Object { if ((Get-KitsOption $_) -eq 'OptionId.KitsConfiguration') { 0 } else { 1 } }
    foreach ($p in $pkgs) {
        $msi = $null
        foreach ($ref in $p.PayloadRef) {
            $pl = $payloads[$ref.Id]
            $dst = Join-Path "$dir\files" $pl.SourcePath
            New-Item -ItemType Directory -Force (Split-Path $dst) | Out-Null
            if (-not (Test-Path $dst)) {
                curl.exe -sSfL -o $dst ($bun.Base + ($pl.SourcePath -replace '\\', '/').Replace(' ', '%20'))
                if ($LASTEXITCODE) { throw "download failed: $($pl.SourcePath)" }
            }
            $hashOk = (Invoke-WithRetry { Get-FileHash $dst -Algorithm SHA1 }).Hash -eq $pl.Hash
            $sig = Invoke-WithRetry { Get-AuthenticodeSignature $dst }
            $sigOk = $sig.Status -eq 'Valid' -and $sig.SignerCertificate.Subject -like 'CN=Microsoft Corporation,*'
            if (-not ($hashOk -or $sigOk)) { throw "UNTRUSTED payload $($pl.SourcePath): hash mismatch, signature $($sig.Status)" }
            if ($pl.SourcePath -like '*.msi') { $msi = $dst }
        }
        $props = ($p.MsiProperty | ForEach-Object {
            $v = $_.Value -replace '\[KITSROOT\]', $kitsRoot -replace '\[UserLocale\]', 'en-US'
            "$($_.Id)=`"$v`""
        }) -join ' '
        $r = Start-Process msiexec.exe -ArgumentList "/i `"$msi`" /qn /norestart $props" -Wait -PassThru
        # KitsConfiguration returns 1603 when the Windows SDK already set KitsRoot10; harmless.
        "{0,-100} exit {1}" -f $p.Id, $r.ExitCode
    }
}
"Done. Check with: woa-builder doctor"
