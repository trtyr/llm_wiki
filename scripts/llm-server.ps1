# llm-server.ps1 - one-click manager for the LLM Wiki headless server.
#
# Usage (from any shell):
#   powershell -ExecutionPolicy Bypass -File scripts\llm-server.ps1            # start
#   powershell -ExecutionPolicy Bypass -File scripts\llm-server.ps1 status
#   powershell -ExecutionPolicy Bypass -File scripts\llm-server.ps1 stop
#
# Commands:
#   start      Start llm-server in the background (default; no-op if already running)
#   stop       Stop the running instance
#   restart    stop + start
#   status     Process state + HTTP health report
#   health     One-shot HTTP /health query
#   logs       Show recent server output (-Tail N, -Follow, -StdOut)
#   start-mcp  Start the MCP HTTP bridge (remote MCP over Streamable HTTP)
#   stop-mcp   Stop the MCP HTTP bridge
#   restart-mcp  stop-mcp + start-mcp
#   build      Build mcp-server + cargo build (checks protoc)
#   help       This text
#
# Options (used by start/restart/build):
#   -BindHost <ip>        LLM_WIKI_BIND_HOST   (default 127.0.0.1)
#   -BindPort <port>      LLM_WIKI_BIND_PORT   (default 19828)
#   -DataDir <path>       LLM_WIKI_DATA_DIR    (default: share with the desktop app)
#   -ProjectPath <path>   LLM_WIKI_PROJECT_PATH (exposed as "current"; scaffolded if missing)
#   -Token <token>        LLM_WIKI_API_TOKEN
#   -Config <path>        JSON config file (default: scripts\llm-server.config.json)
#   -Binary <path>        Explicit llm-server.exe path (default: newest build)
#   -Release              build flag for the "build" command
#
# MCP bridge options (start-mcp/restart-mcp; also via config file):
#   -McpHttp <0|1>        enable/disable the bridge (default 0)
#   -McpHost <ip>         bind address for the bridge (default 127.0.0.1)
#   -McpPort <port>       bridge port (default 8931)
# The bridge exposes http://<McpHost>:<McpPort>/mcp (Streamable HTTP, Bearer
# token auth reusing -Token) and forwards to the local API.
#
# Config file: copy scripts\llm-server.config.example.json to
# scripts\llm-server.config.json and edit it. CLI options override the file.

param(
    [Parameter(Position = 0)]
    [string]$Command = "start",
    [string]$BindHost = "",
    [int]$BindPort = 0,
    [string]$DataDir = "",
    [string]$ProjectPath = "",
    [string]$Token = "",
    [string]$Config = "",
    [string]$Binary = "",
    [string]$McpHttp = "",
    [string]$McpHost = "",
    [int]$McpPort = 0,
    [switch]$Release,
    [int]$Tail = 40,
    [switch]$Follow,
    [switch]$StdOut
)

$ErrorActionPreference = "Stop"

if ($PSVersionTable.Platform -eq "Unix") {
    Write-Error "This script manages the Windows llm-server build. On Linux/macOS run the binary directly."
}

$RepoRoot = Split-Path -Parent $PSScriptRoot
$RunDir = Join-Path $RepoRoot ".run"
$PidFile = Join-Path $RunDir "llm-server.pid"
$OutLog = Join-Path $RunDir "llm-server.out.log"
$ErrLog = Join-Path $RunDir "llm-server.err.log"
$McpPidFile = Join-Path $RunDir "mcp-http.pid"
$McpOutLog = Join-Path $RunDir "mcp-http.out.log"
$McpErrLog = Join-Path $RunDir "mcp-http.err.log"
$DefaultConfigPath = Join-Path $PSScriptRoot "llm-server.config.json"

# ---------------------------------------------------------------------------
# Configuration: defaults < config file < last-start snapshot < CLI parameters
# ---------------------------------------------------------------------------

$Settings = @{
    BindHost   = "127.0.0.1"
    BindPort   = 19828
    DataDir    = ""
    ProjectPath = ""
    Token      = ""
    ProtocPath = ""
    McpHttp    = "0"
    McpHost    = "127.0.0.1"
    McpPort    = 8931
}
$StringKeys = @("BindHost", "DataDir", "ProjectPath", "Token", "ProtocPath", "McpHttp", "McpHost")
$IntKeys = @("BindPort", "McpPort")

function Merge-Settings {
    param($Source)
    foreach ($key in $script:StringKeys) {
        if ($Source.PSObject.Properties.Name -contains $key) {
            $value = $Source.$key
            if ("$value" -ne "") { $script:Settings[$key] = "$value" }
        }
    }
    foreach ($key in $script:IntKeys) {
        if ($Source.PSObject.Properties.Name -contains $key) {
            $intValue = 0
            if ([int]::TryParse("$($Source.$key)", [ref]$intValue) -and $intValue -gt 0) {
                $script:Settings[$key] = $intValue
            }
        }
    }
}

$configPath = if ($Config -ne "") { $Config } else { $DefaultConfigPath }
if (Test-Path $configPath) {
    Merge-Settings (Get-Content $configPath -Raw | ConvertFrom-Json)
}

# status/health/stop/logs/restart describe an instance that may have been
# started with different settings than today's defaults — prefer the snapshot
# written when it was last started. Explicit CLI parameters still win, so
# `restart -BindPort X` re-binds as requested.
$RuntimePath = Join-Path $RunDir "llm-server.runtime.json"
if ((Test-Path $RuntimePath) -and @("status", "health", "stop", "logs", "restart", "start-mcp", "stop-mcp", "restart-mcp") -contains $Command.ToLower()) {
    Merge-Settings (Get-Content $RuntimePath -Raw | ConvertFrom-Json)
}

# CLI parameters win when they were actually passed in. $PSBoundParameters
# is a Dictionary — a direct [pscustomobject] cast would expose the
# dictionary's own properties (Count, Keys, ...) instead of its entries,
# so copy into a hashtable first.
$cliParams = @{}
foreach ($key in $PSBoundParameters.Keys) {
    $cliParams[$key] = $PSBoundParameters[$key]
}
Merge-Settings ([pscustomobject]$cliParams)

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

function Get-ServerBinary {
    if ($Binary -ne "") {
        if (-not (Test-Path $Binary)) { Write-Error "Binary not found: $Binary" }
        return (Resolve-Path $Binary).Path
    }
    $candidates = @(
        (Join-Path $RepoRoot "src-tauri\target\release\llm-server.exe"),
        (Join-Path $RepoRoot "src-tauri\target\debug\llm-server.exe")
    )
    foreach ($candidate in $candidates) {
        if (Test-Path $candidate) { return $candidate }
    }
    Write-Error ("llm-server.exe not found. Run: scripts\llm-server.ps1 build " +
                 "(or pass -Binary <path>)")
}

# Health must be queried on a reachable address even when the server binds
# to a wildcard host.
function Get-HealthBaseUrl {
    $hostName = $Settings.BindHost
    if ($hostName -eq "0.0.0.0" -or $hostName -eq "[::]" -or $hostName -eq "::") {
        $hostName = "127.0.0.1"
    }
    "http://$hostName`:$($Settings.BindPort)"
}

function Get-ServerProcess {
    if (Test-Path $PidFile) {
        $recordedPid = Get-Content $PidFile -TotalCount 1
        if ("$recordedPid" -match "^\d+$") {
            $proc = Get-Process -Id ([int]$recordedPid) -ErrorAction SilentlyContinue
            if ($proc -and $proc.ProcessName -eq "llm-server") { return @($proc) }
        }
        Remove-Item $PidFile -ErrorAction SilentlyContinue
    }
    # Started outside this script (or pid recycled): fall back to image name.
    $procs = Get-Process -Name "llm-server" -ErrorAction SilentlyContinue
    if ($procs) { return @($procs) }
    return $null
}

function Invoke-Health {
    try {
        Invoke-RestMethod -Uri "$(Get-HealthBaseUrl)/health" -TimeoutSec 3
    } catch {
        $null
    }
}

function Format-SettingsLine {
    $parts = @("bind=$($Settings.BindHost):$($Settings.BindPort)")
    if ($Settings.DataDir -ne "") { $parts += "data=$($Settings.DataDir)" }
    if ($Settings.ProjectPath -ne "") { $parts += "project=$($Settings.ProjectPath)" }
    $parts += if ($Settings.Token -ne "") { "token=<set>" } else { "token=<none>" }
    $parts -join "  "
}

# Start-Process has no -Environment on Windows PowerShell 5.1; set the
# process-level env vars around the spawn, then restore, so the caller's
# shell stays clean.
function Start-ServerProcess {
    $binaryPath = Get-ServerBinary
    $existing = Get-ServerProcess
    if ($existing) {
        Write-Host "llm-server is already running (pid $($existing[0].Id)). Use 'restart' to reload config."
        return
    }
    if (-not (Test-Path $RunDir)) { New-Item -ItemType Directory -Path $RunDir | Out-Null }

    $envVars = [ordered]@{}
    $envVars["LLM_WIKI_BIND_HOST"] = $Settings.BindHost
    $envVars["LLM_WIKI_BIND_PORT"] = "$($Settings.BindPort)"
    if ($Settings.DataDir -ne "") { $envVars["LLM_WIKI_DATA_DIR"] = $Settings.DataDir }
    if ($Settings.ProjectPath -ne "") { $envVars["LLM_WIKI_PROJECT_PATH"] = $Settings.ProjectPath }
    if ($Settings.Token -ne "") { $envVars["LLM_WIKI_API_TOKEN"] = $Settings.Token }

    $saved = @{}
    foreach ($key in $envVars.Keys) {
        $saved[$key] = [Environment]::GetEnvironmentVariable($key)
        [Environment]::SetEnvironmentVariable($key, $envVars[$key], "Process")
    }
    try {
        $proc = Start-Process -FilePath $binaryPath `
            -WorkingDirectory $RepoRoot `
            -WindowStyle Hidden `
            -RedirectStandardOutput $OutLog `
            -RedirectStandardError $ErrLog `
            -PassThru
    } finally {
        foreach ($key in $saved.Keys) {
            [Environment]::SetEnvironmentVariable($key, $saved[$key], "Process")
        }
    }

    Start-Sleep -Seconds 2
    $proc.Refresh()
    if ($proc.HasExited) {
        Write-Host "llm-server exited immediately (code $($proc.ExitCode)). Last log lines:" -ForegroundColor Red
        Get-Content $ErrLog -Tail 15 -ErrorAction SilentlyContinue | Write-Host
        return
    }
    Set-Content -Path $PidFile -Value $proc.Id | Out-Null
    # Snapshot the effective settings so status/health/stop/logs describe
    # this instance even after the shell is gone.
    $Settings | ConvertTo-Json | Set-Content -Path $RuntimePath
    Write-Host "llm-server started (pid $($proc.Id))" -ForegroundColor Green
    Write-Host "  $(Format-SettingsLine)"
    Write-Host "  API:   $(Get-HealthBaseUrl)/api/v1"
    Write-Host "  Logs:  $ErrLog"
    $health = Invoke-Health
    if ($health) {
        Write-Host "  Health: running, version $($health.version), authRequired=$($health.authRequired)"
    } else {
        Write-Host "  Health: not responding yet (check 'logs')" -ForegroundColor Yellow
    }
    if ($Settings.BindHost -notin @("127.0.0.1", "localhost", "::1", "[::1]") -and $Settings.Token -eq "") {
        Write-Host "  WARNING: non-loopback bind without a token. Agent chat can run shell commands on this machine." -ForegroundColor Yellow
    }
}

function Stop-ServerProcess {
    $procs = Get-ServerProcess
    if (-not $procs) {
        Write-Host "llm-server is not running."
        return
    }
    foreach ($proc in $procs) {
        # Hard kill: server state (ingest queue, agent sessions) is
        # crash-safe by design; there is no console to deliver Ctrl-C to.
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        Wait-Process -Id $proc.Id -Timeout 5 -ErrorAction SilentlyContinue
        if ($proc.HasExited) {
            Write-Host "llm-server (pid $($proc.Id)) stopped."
        } else {
            Write-Host "llm-server (pid $($proc.Id)) did not exit in time." -ForegroundColor Yellow
        }
    }
    Remove-Item $PidFile -ErrorAction SilentlyContinue
}

function Show-Status {
    $procs = Get-ServerProcess
    if (-not $procs) {
        Write-Host "llm-server: STOPPED"
        Write-Host "  config: $(Format-SettingsLine)"
        Write-Host "  start:  scripts\llm-server.ps1 start"
        return
    }
    Write-Host "llm-server: RUNNING (pid $($procs[0].Id), started $($procs[0].StartTime))"
    if ($procs.Count -gt 1) {
        Write-Host "  NOTE: $($procs.Count) llm-server processes found: $($procs.Id -join ', ')" -ForegroundColor Yellow
    }
    $health = Invoke-Health
    if ($health) {
        Write-Host "  health: ok (version $($health.version), status $($health.status))"
        Write-Host "          authRequired=$($health.authRequired) authConfigured=$($health.authConfigured) tokenSource=$($health.tokenSource)"
        Write-Host "          enabled=$($health.enabled) allowLanAccess=$($health.allowLanAccess)"
    } else {
        Write-Host "  health: no response on $(Get-HealthBaseUrl)/health" -ForegroundColor Yellow
    }
    $mcp = Get-McpProcess
    if ($mcp) {
        Write-Host "  mcp:    RUNNING (pid $($mcp.Id)) http://$($Settings.McpHost):$($Settings.McpPort)/mcp"
    } elseif ($Settings.McpHttp -eq "1") {
        Write-Host "  mcp:    STOPPED (start with 'start-mcp')" -ForegroundColor Yellow
    }
}

function Show-Logs {
    $target = if ($StdOut) { $OutLog } else { $ErrLog }
    if (-not (Test-Path $target)) {
        Write-Host "No log file yet: $target"
        return
    }
    if ($Follow) {
        Get-Content $target -Tail $Tail -Wait
    } else {
        Get-Content $target -Tail $Tail
    }
}

function Get-McpProcess {
    # Pid-file only: the bridge is a generic node.exe, so a by-image-name
    # fallback could match unrelated Node processes.
    if (Test-Path $McpPidFile) {
        $recordedPid = Get-Content $McpPidFile -TotalCount 1
        if ("$recordedPid" -match "^\d+$") {
            $proc = Get-Process -Id ([int]$recordedPid) -ErrorAction SilentlyContinue
            if ($proc -and $proc.ProcessName -eq "node") { return $proc }
        }
        Remove-Item $McpPidFile -ErrorAction SilentlyContinue
    }
    return $null
}

function Start-McpBridge {
    if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
        Write-Error "node not found on PATH (the bridge is a Node script)."
    }
    if (-not (Test-Path (Join-Path $RepoRoot "mcp-server\dist\src\http.js"))) {
        Write-Error "mcp-server HTTP entry is not built. Run: scripts\llm-server.ps1 build"
    }
    $existing = Get-McpProcess
    if ($existing) {
        Write-Host "mcp-http bridge is already running (pid $($existing.Id))."
        return
    }
    if (-not (Get-ServerProcess)) {
        Write-Host "WARNING: llm-server is not running; the bridge will start but tools will fail until it is." -ForegroundColor Yellow
    }
    if (-not (Test-Path $RunDir)) { New-Item -ItemType Directory -Path $RunDir | Out-Null }

    $envVars = [ordered]@{
        LLM_WIKI_API_TOKEN  = $Settings.Token
        LLM_WIKI_API_BASE_URL = "http://127.0.0.1:$($Settings.BindPort)"
        MCP_HTTP_HOST       = $Settings.McpHost
        MCP_HTTP_PORT       = "$($Settings.McpPort)"
    }
    $saved = @{}
    foreach ($key in $envVars.Keys) {
        $saved[$key] = [Environment]::GetEnvironmentVariable($key)
        [Environment]::SetEnvironmentVariable($key, $envVars[$key], "Process")
    }
    try {
        $proc = Start-Process -FilePath "node" `
            -ArgumentList (Join-Path $RepoRoot "mcp-server\dist\src\http.js") `
            -WorkingDirectory (Join-Path $RepoRoot "mcp-server") `
            -WindowStyle Hidden `
            -RedirectStandardOutput $McpOutLog `
            -RedirectStandardError $McpErrLog `
            -PassThru
    } finally {
        foreach ($key in $saved.Keys) {
            [Environment]::SetEnvironmentVariable($key, $saved[$key], "Process")
        }
    }
    Start-Sleep -Seconds 2
    $proc.Refresh()
    if ($proc.HasExited) {
        Write-Host "mcp-http bridge exited immediately. Last log lines:" -ForegroundColor Red
        Get-Content $McpErrLog -Tail 15 -ErrorAction SilentlyContinue | Write-Host
        return
    }
    Set-Content -Path $McpPidFile -Value $proc.Id | Out-Null
    Write-Host "mcp-http bridge started (pid $($proc.Id))" -ForegroundColor Green
    Write-Host "  URL:   http://$($Settings.McpHost):$($Settings.McpPort)/mcp"
    Write-Host "  Auth:  Authorization: Bearer <Token>"
    Write-Host "  Logs:  $McpErrLog"
}

function Stop-McpBridge {
    $proc = Get-McpProcess
    if (-not $proc) {
        Write-Host "mcp-http bridge is not running."
        return
    }
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    Wait-Process -Id $proc.Id -Timeout 5 -ErrorAction SilentlyContinue
    Remove-Item $McpPidFile -ErrorAction SilentlyContinue
    Write-Host "mcp-http bridge (pid $($proc.Id)) stopped."
}

function Invoke-Build {
    Write-Host "[1/3] mcp-server bundle (required by tauri build script)"
    $mcpDir = Join-Path $RepoRoot "mcp-server"
    if (-not (Test-Path (Join-Path $mcpDir "node_modules"))) {
        Push-Location $mcpDir
        npm ci --no-audit --no-fund
        Pop-Location
        if ($LASTEXITCODE -ne 0) { Write-Error "npm ci failed in mcp-server" }
    }
    Push-Location $mcpDir
    npm run build
    Pop-Location
    if ($LASTEXITCODE -ne 0) { Write-Error "mcp-server build failed" }

    Write-Host "[2/3] protoc (needed by lance-encoding)"
    # Cast to string everywhere: an unset env var is $null, and Test-Path
    # rejects both $null and "" outright.
    $protoc = "$($Settings.ProtocPath)"
    if ($protoc -eq "" -and "$($env:PROTOC)" -ne "") { $protoc = "$($env:PROTOC)" }
    $protocFound = ($protoc -ne "" -and (Test-Path $protoc))
    if (-not $protocFound) {
        $onPath = Get-Command protoc -ErrorAction SilentlyContinue
        if ($onPath -and $onPath.Source -ne "") {
            $protoc = $onPath.Source
            $protocFound = $true
        }
    }
    if (-not $protocFound) {
        Write-Error ("protoc not found. Install it once, e.g. 'winget install Google.Protobuf', " +
                     "or set ProtocPath in the config file / the PROTOC env var.")
    }
    $env:PROTOC = $protoc
    Write-Host "  using protoc: $protoc"

    Write-Host "[3/3] cargo build"
    # tauri's context macro requires the frontend dist dir to exist even
    # for headless builds; a placeholder satisfies it and is overwritten
    # by any real frontend build.
    $frontendDist = Join-Path $RepoRoot "dist"
    if (-not (Test-Path (Join-Path $frontendDist "index.html"))) {
        New-Item -ItemType Directory -Force -Path $frontendDist | Out-Null
        Set-Content -Path (Join-Path $frontendDist "index.html") -Value "<!doctype html><title>placeholder</title>"
    }
    Push-Location (Join-Path $RepoRoot "src-tauri")
    if ($Release) { cargo build --release } else { cargo build }
    Pop-Location
    if ($LASTEXITCODE -ne 0) { Write-Error "cargo build failed" }
    Write-Host "build ok." -ForegroundColor Green
}

function Show-Help {
    # The usage header is the leading block of # lines in this file.
    Get-Content $PSCommandPath -TotalCount 46 |
        Where-Object { $_ -match '^#' } |
        ForEach-Object { $_ -replace '^#\s?', '' }
}

# ---------------------------------------------------------------------------
# Dispatch
# ---------------------------------------------------------------------------

switch ($Command.ToLower()) {
    "start"       { Start-ServerProcess }
    "stop"        { Stop-ServerProcess }
    "restart"     { Stop-ServerProcess; Start-Sleep -Seconds 1; Start-ServerProcess }
    "status"      { Show-Status }
    "health"      {
        $health = Invoke-Health
        if ($health) { $health | ConvertTo-Json -Depth 5 }
        else { Write-Host "no response from $(Get-HealthBaseUrl)/health" }
    }
    "logs"        { Show-Logs }
    "start-mcp"   { Start-McpBridge }
    "stop-mcp"    { Stop-McpBridge }
    "restart-mcp" { Stop-McpBridge; Start-Sleep -Seconds 1; Start-McpBridge }
    "build"       { Invoke-Build }
    "help"        { Show-Help }
    default       { Write-Host "Unknown command: $Command"; Show-Help }
}
