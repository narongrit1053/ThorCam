# Builds the release program as one file: dist\ThorCamBeamProfiler.exe, with the Thorlabs SDK DLLs inside it.
# Run from this folder:  powershell -ExecutionPolicy Bypass -File .\package.ps1
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
. (Join-Path $here '..\.rust\env.ps1')

Push-Location $here
try {
    cargo build --release -p thorcam
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

    $dist = Join-Path $here 'dist'
    New-Item -ItemType Directory -Force $dist | Out-Null
    # The folder layout of earlier versions (exe + Library_X64) is replaced by the single file.
    $old = Join-Path $dist 'ThorCam'
    if (Test-Path $old) { Remove-Item -Recurse -Force $old }
    $exe = Join-Path $dist 'ThorCamBeamProfiler.exe'
    Copy-Item (Join-Path $here 'target\release\thorcam.exe') $exe -Force
    Write-Host ("Done: {0} ({1:N1} MB)" -f $exe, ((Get-Item $exe).Length / 1MB))
}
finally {
    Pop-Location
}
