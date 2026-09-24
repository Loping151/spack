param([switch]$DebugBuild)
$ErrorActionPreference = 'Stop'
$env:CARGO_TARGET_DIR = Join-Path $env:LOCALAPPDATA 'spack-target'
Push-Location (Split-Path $PSScriptRoot -Parent)
try {
    npm ci
    if ($LASTEXITCODE -ne 0) { throw 'npm ci failed' }
    npm run check
    if ($LASTEXITCODE -ne 0) { throw 'checks failed' }
    if ($DebugBuild) { npm run tauri dev }
    else {
        npm run build
        if ($LASTEXITCODE -ne 0) { throw 'build failed' }
        New-Item -ItemType Directory -Force -Path dist | Out-Null
        Copy-Item -LiteralPath (Join-Path $env:CARGO_TARGET_DIR 'release\spack.exe') -Destination 'dist\spack.exe'
        Get-FileHash -LiteralPath 'dist\spack.exe' -Algorithm SHA256
    }
} finally { Pop-Location }
