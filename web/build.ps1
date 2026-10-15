# Builds the WebAssembly module into web/wasm/. The web/ folder is then a complete static site.
#   .\web\build.ps1            build
#   .\web\build.ps1 -Serve     build, then serve http://localhost:8000 (WebUSB also works on localhost)
param([switch]$Serve)
# Not 'Stop': Windows PowerShell treats cargo's progress output on stderr as an error. Exit codes are checked.
$workspace = Split-Path -Parent $PSScriptRoot
# The project-local toolchain, when there is one (it sets its own $root, so keep ours under another name).
$envScript = Join-Path (Split-Path -Parent $workspace) '.rust\env.ps1'
if (Test-Path $envScript) { . $envScript }
Push-Location $workspace
try {
    cargo build -p beam-wasm --target wasm32-unknown-unknown --release
    if ($LASTEXITCODE) { throw 'cargo build failed' }
    New-Item -ItemType Directory -Force (Join-Path $PSScriptRoot 'wasm') | Out-Null
    Copy-Item 'target\wasm32-unknown-unknown\release\beam_wasm.wasm' (Join-Path $PSScriptRoot 'wasm\beam_wasm.wasm') -Force
    Write-Host "Built web\wasm\beam_wasm.wasm"
} finally { Pop-Location }
if ($Serve) {
    Write-Host 'Serving http://localhost:8000  (Ctrl+C to stop)'
    python -m http.server 8000 --bind 127.0.0.1 --directory $PSScriptRoot
}
