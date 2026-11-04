# Cross-compile llm-server for Linux and (optionally) deploy to a server.
# Requires: rustup target x86_64-unknown-linux-gnu, zig (winget zig.zig),
# cargo-zigbuild (cargo install cargo-zigbuild --locked), protoc.
#
# Usage:
#   powershell -File scripts/build-linux.ps1                # build only
#   powershell -File scripts/build-linux.ps1 -Deploy        # build + scp to beijing + restart
#   powershell -File scripts/build-linux.ps1 -Host beijing -RemoteDir /opt/llm-server -Deploy
param(
    [switch]$Release,
    [switch]$Deploy,
    [string]$Host = "beijing",
    [string]$RemoteDir = "/opt/llm-server",
    [string]$BindHost = "0.0.0.0",
    [int]$BindPort = 19828
)
$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
$Target = "x86_64-unknown-linux-gnu.2.17"   # glibc 2.17+ floor; beijing-hub runs 2.38

# zig (from winget) for the cross linker
$zig = Get-ChildItem "$env:LOCALAPPDATA/Microsoft/WinGet/Packages" -Recurse -Filter zig.exe `
        -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty FullName
if (-not $zig) { Write-Error "zig.exe not found - install with: winget install zig.zig" }
$env:PATH = (Split-Path $zig) + ";" + $env:PATH
Write-Host "[1/3] zig $($(& $zig version))"

# protoc (lance-encoding requirement; path saved by llm-server.ps1 config)
$cfg = Get-Content (Join-Path $PSScriptRoot "llm-server.config.json") -ErrorAction SilentlyContinue | ConvertFrom-Json
if ("$($cfg.ProtocPath)" -ne "" -and (Test-Path $cfg.ProtocPath)) { $env:PROTOC = $cfg.ProtocPath }
elseif ($env:PROTOC -eq "") {
    $p = Get-Command protoc -ErrorAction SilentlyContinue
    if ($p) { $env:PROTOC = $p.Source }
}
if (-not $env:PROTOC) { Write-Error "protoc not found (set ProtocPath in llm-server.config.json)" }
Write-Host "[2/3] protoc: $env:PROTOC"

Write-Host "[3/3] cargo zigbuild --release --target $Target"
Push-Location (Join-Path $RepoRoot "src-tauri")
cargo zigbuild --release --target $Target --bin llm-server
$exit = $LASTEXITCODE
Pop-Location
if ($exit -ne 0) { Write-Error "zigbuild failed" }

$bin = Join-Path $RepoRoot "src-tauri/target/x86_64-unknown-linux-gnu/release/llm-server"
Write-Host "OK: $bin ($([math]::Round((Get-Item $bin).Length/1MB)) MB)"

if ($Deploy) {
    Write-Host "Deploying to $Host`:$RemoteDir ..."
    ssh $Host "mkdir -p $RemoteDir"
    scp $bin "${Host}:${RemoteDir}/llm-server.new"
    ssh $Host "chmod +x $RemoteDir/llm-server.new; " +
        "if pgrep -f 'llm-server' > /dev/null; then kill (pgrep -f 'llm-server' | head -1); sleep 1; end; " +
        "mv $RemoteDir/llm-server.new $RemoteDir/llm-server"
    # NOTE: fish-style括号不通用——实际部署时用 bash -c 显式执行
    ssh $Host "bash -c 'if pgrep -f llm-server >/dev/null; then kill `$(pgrep -f llm-server | head -1); sleep 1; fi; mv $RemoteDir/llm-server.new $RemoteDir/llm-server'"
    Write-Host "Deployed. Start it with:"
    Write-Host "  ssh $Host `"cd $RemoteDir && LLM_WIKI_BIND_HOST=$BindHost LLM_WIKI_BIND_PORT=$BindPort LLM_WIKI_API_TOKEN=<token> LLM_WIKI_DATA_DIR=$RemoteDir/data nohup ./llm-server > server.log 2>&1 &`""
}
