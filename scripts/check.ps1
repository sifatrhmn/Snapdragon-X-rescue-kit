$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

function Invoke-Step {
    param([string]$Name, [scriptblock]$Cmd)
    Write-Host "==> $Name"
    & $Cmd
    if ($LASTEXITCODE -ne 0) {
        Write-Error "Step failed: $Name (exit $LASTEXITCODE)"
        exit $LASTEXITCODE
    }
}

Invoke-Step 'cargo fmt'    { cargo fmt --all --check }
Invoke-Step 'cargo clippy' { cargo clippy --workspace --all-targets -- -D warnings }
Invoke-Step 'cargo test'   { cargo test --workspace }
Invoke-Step 'cargo build (aarch64 release)' { cargo build --workspace --release --target aarch64-pc-windows-msvc }

Write-Host 'All checks passed.'
