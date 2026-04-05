# install.ps1 — Install octos from pre-built binaries on Windows.
# Self-contained: no repo clone, Rust, or Node.js needed.
#
# Usage:
#   irm https://github.com/octos-org/octos/releases/latest/download/install.ps1 | iex
#
#   # Or download and run with options:
#   .\install.ps1 -Version v0.5.0
#   .\install.ps1 -Doctor
#   .\install.ps1 -Uninstall
#
# Environment variables (for piped installs):
#   $env:OCTOS_VERSION   — Release version (default: latest)
#   $env:OCTOS_PREFIX    — Install prefix (default: ~\.octos\bin)

[CmdletBinding(DefaultParameterSetName = 'Install')]
param(
    [Parameter(ParameterSetName = 'Install')]
    [string]$Version = "",

    [Parameter(ParameterSetName = 'Install')]
    [string]$Prefix = "",

    [Parameter(ParameterSetName = 'Install')]
    [string]$AuthToken = "",

    [Parameter(ParameterSetName = 'Doctor', Mandatory)]
    [switch]$Doctor,

    [Parameter(ParameterSetName = 'Uninstall', Mandatory)]
    [switch]$Uninstall,

    [Parameter(ParameterSetName = 'Install')]
    [string]$Domain = "",

    [Parameter(ParameterSetName = 'Install')]
    [int]$Port = 8080,

    [Parameter(ParameterSetName = 'Install')]
    [switch]$InstallDeps,

    [Parameter(ParameterSetName = 'Install')]
    [switch]$Tunnel,

    [Parameter(ParameterSetName = 'Install')]
    [string]$TenantName = "",

    [Parameter(ParameterSetName = 'Install')]
    [string]$FrpsToken = "",

    [Parameter(ParameterSetName = 'Install')]
    [string]$FrpsTokenFile = "",

    [Parameter(ParameterSetName = 'Install')]
    [string]$FrpsServer = "",

    [Parameter(ParameterSetName = 'Install')]
    [int]$SshPort = 0,

    [Parameter(ParameterSetName = 'Install')]
    [string]$TunnelDomain = "",

    [Parameter(ParameterSetName = 'Help')]
    [Alias("h")]
    [switch]$Help
)

$ErrorActionPreference = "Stop"

# ── Help ─────────────────────────────────────────────────────────────
if ($Help) {
    Write-Host @"
install.ps1 — Install octos from pre-built binaries on Windows.

USAGE
  Piped:     irm https://github.com/octos-org/octos/releases/latest/download/install.ps1 | iex
  Download:  .\install.ps1 [options]
  Tunnel:    .\install.ps1 -Tunnel -TenantName alice -FrpsToken <token>

OPTIONS
  -Version <tag>       Release version (default: latest)
  -Prefix <path>       Install prefix (default: ~\.octos\bin)
  -Port <port>         octos serve port (default: 8080)
  -AuthToken <token>   Auth token for octos serve (default: auto-generated)
  -Doctor              Diagnose installation and service health
  -Uninstall           Remove octos binaries, services, and PATH entry
  -Help                Show this help message

OPTIONAL FEATURES
  -InstallDeps         Auto-install missing runtime dependencies
  -Domain <domain>     Set up Caddy reverse proxy with on-demand TLS

TUNNEL (frpc)
  -Tunnel                Enable frpc tunnel (also enabled by -TenantName/-FrpsToken)
  -TenantName <name>     Tenant subdomain (e.g. "alice")
  -FrpsToken <token>     frps auth token
  -FrpsTokenFile <file>  Read frps auth token from file
  -FrpsServer <addr>     frps server address (default: 163.192.33.32)
  -SshPort <port>        SSH tunnel remote port (default: 6001)
  -TunnelDomain <domain> Tunnel domain (default: octos-cloud.org)

ENVIRONMENT VARIABLES
  OCTOS_VERSION      Release version override
  OCTOS_PREFIX       Install prefix override
  OCTOS_HOME         Data directory override (default: ~\.octos)
  OCTOS_AUTH_TOKEN   Auth token override
  OCTOS_DOWNLOAD_URL Local/self-hosted download directory
"@
    exit 0
}

# ── Defaults ──────────────────────────────────────────────────────────
$GithubRepo = "octos-org/octos"

if (-not $Version)   { $Version = if ($env:OCTOS_VERSION) { $env:OCTOS_VERSION } else { "latest" } }
if (-not $Prefix)    { $Prefix  = if ($env:OCTOS_PREFIX)  { $env:OCTOS_PREFIX }  else { Join-Path $HOME ".octos\bin" } }

$DataDir = if ($env:OCTOS_HOME) { $env:OCTOS_HOME } else { Join-Path $HOME ".octos" }

# ── Tunnel defaults ──────────────────────────────────────────────────
$FrpcVersion = "0.61.1"
if (-not $FrpsServer)   { $FrpsServer   = "163.192.33.32" }
if ($SshPort -eq 0)     { $SshPort      = 6001 }
if (-not $TunnelDomain) { $TunnelDomain = "octos-cloud.org" }

# Auto-enable tunnel when tunnel-specific args are passed
if ($TenantName -or $FrpsToken -or $FrpsTokenFile) { $Tunnel = [switch]::new($true) }

# Resolve frps token from file
if (-not $FrpsToken -and $FrpsTokenFile) {
    if (Test-Path $FrpsTokenFile) {
        $FrpsToken = (Get-Content $FrpsTokenFile -Raw).Trim()
    } else {
        Write-Host "    ERROR: token file not found: $FrpsTokenFile" -ForegroundColor Red
        exit 1
    }
}

# Derived paths (depend on $Prefix/$DataDir which are set above)
$FrpcBin    = Join-Path $Prefix "frpc.exe"
$FrpcConfig = Join-Path $DataDir "frpc.toml"
$FrpcLog    = Join-Path $DataDir "logs\frpc.log"

# ── Helpers ───────────────────────────────────────────────────────────
# (Validate-Inputs is called after helper definitions below)
function Section($msg) { Write-Host "`n==> $msg" }
function Ok($msg)      { Write-Host "    OK: $msg" }
function Warn($msg)    { Write-Host "    WARN: $msg" -ForegroundColor Yellow }
function Hint($msg)    { Write-Host "          -> $msg" }
function Err($msg) {
    if ($Doctor) {
        Write-Host "    FAIL: $msg" -ForegroundColor Red
        $script:DoctorIssues++
    } else {
        Write-Host "    ERROR: $msg" -ForegroundColor Red
        Write-Host ""
        Write-Host "    Run diagnostics:"
        Write-Host "      irm https://github.com/octos-org/octos/releases/latest/download/install.ps1 -OutFile install.ps1; .\install.ps1 -Doctor"
        exit 1
    }
}

function Test-Command($cmd) {
    $null -ne (Get-Command $cmd -ErrorAction SilentlyContinue)
}

# Validate a value against a regex pattern; exit on mismatch.
function Validate($name, $value, $pattern) {
    if ($value -and $value -notmatch "^${pattern}$") {
        Write-Host "    ERROR: invalid ${name}: '${value}'" -ForegroundColor Red
        Write-Host "           Must match: ${pattern}"
        exit 1
    }
}

function Validate-Inputs {
    if ($AuthToken) { Validate "auth-token" $AuthToken '[a-zA-Z0-9._-]+' }
    if ($Domain)    { Validate "domain"     $Domain    '[a-zA-Z0-9.-]+' }
    if ($Version -and $Version -ne "latest") { Validate "version" $Version '[a-zA-Z0-9._-]+' }
    if ($Port)      { Validate "port"       $Port      '[0-9]+' }
    if ($TenantName)   { Validate "tenant-name"   $TenantName   '[a-zA-Z0-9]([a-zA-Z0-9-]*[a-zA-Z0-9])?' }
    if ($FrpsToken)    { Validate "frps-token"    $FrpsToken    '[a-zA-Z0-9._-]+' }
    if ($FrpsServer)   { Validate "frps-server"   $FrpsServer   '[a-zA-Z0-9.:-]+' }
    if ($SshPort)      { Validate "ssh-port"      $SshPort      '[0-9]+' }
    if ($TunnelDomain) { Validate "tunnel-domain" $TunnelDomain '[a-zA-Z0-9.-]+' }
}

# Print install hint for a package on Windows
function Get-PkgHint($pkg) {
    switch ($pkg) {
        "git"      { "winget install Git.Git" }
        "node"     { "winget install OpenJS.NodeJS.LTS" }
        "python"   { "winget install Python.Python.3.12" }
        "chromium" { "winget install Google.Chrome" }
        "ffmpeg"   { "winget install Gyan.FFmpeg" }
        default    { "install '$pkg' via winget, choco, or scoop" }
    }
}

function Get-WindowsArchitecture() {
    if ($env:PROCESSOR_ARCHITEW6432) {
        return $env:PROCESSOR_ARCHITEW6432.ToUpperInvariant()
    }

    if ($env:PROCESSOR_ARCHITECTURE) {
        return $env:PROCESSOR_ARCHITECTURE.ToUpperInvariant()
    }

    return [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToUpperInvariant()
}

# ── frpc tunnel helpers ──────────────────────────────────────────────

# Write frpc.toml to $DataDir\frpc.toml.
function Write-FrpcConfig {
    $toml = @"
serverAddr = "$FrpsServer"
serverPort = 7000
auth.method = "token"
auth.token = "$FrpsToken"
log.to = "$($FrpcLog -replace '\\', '/')"
log.level = "info"
log.maxDays = 7

[[proxies]]
name = "$TenantName-web"
type = "http"
localPort = $Port
customDomains = ["$TenantName.$TunnelDomain"]

[[proxies]]
name = "$TenantName-ssh"
type = "tcp"
localIP = "127.0.0.1"
localPort = 22
remotePort = $SshPort
"@
    # Ensure logs directory exists
    $logsDir = Join-Path $DataDir "logs"
    if (-not (Test-Path $logsDir)) {
        New-Item -ItemType Directory -Path $logsDir -Force | Out-Null
    }
    [System.IO.File]::WriteAllText($FrpcConfig, $toml, [System.Text.UTF8Encoding]::new($false))
}

# Download and install frpc binary to $Prefix.
function Install-FrpcBinary {
    if (Test-Path $FrpcBin) {
        try {
            $ver = & $FrpcBin --version 2>&1 | Select-Object -First 1
            Ok "frpc already installed ($ver)"
        } catch {
            Ok "frpc already installed (version unknown)"
        }
        return
    }
    Write-Host "    Installing frpc v${FrpcVersion}..."
    $frpZip = "frp_${FrpcVersion}_windows_amd64.zip"
    $frpUrl = "https://github.com/fatedier/frp/releases/download/v${FrpcVersion}/$frpZip"
    $frpTmp = Join-Path ([System.IO.Path]::GetTempPath()) "frpc-install-$([System.Guid]::NewGuid().ToString('N').Substring(0,8))"
    New-Item -ItemType Directory -Path $frpTmp -Force | Out-Null
    try {
        $zipPath = Join-Path $frpTmp $frpZip
        Invoke-WebRequest -Uri $frpUrl -OutFile $zipPath -UseBasicParsing
        Expand-Archive -Path $zipPath -DestinationPath $frpTmp -Force
        $frpcSrc = Get-ChildItem -Path $frpTmp -Recurse -Filter "frpc.exe" | Select-Object -First 1
        if (-not $frpcSrc) { Err "frpc.exe not found in downloaded archive"; return }
        Copy-Item $frpcSrc.FullName -Destination $FrpcBin -Force
        Ok "frpc installed"
    } finally {
        Remove-Item -Recurse -Force $frpTmp -ErrorAction SilentlyContinue
    }
}

# Register and start frpc as a Windows Service.
function Install-FrpcService {
    # Clean up prior registration (silently)
    & $FrpcBin uninstall 2>$null
    Start-Sleep -Seconds 1

    # Register service with config path
    & $FrpcBin install -c $FrpcConfig
    if ($LASTEXITCODE -ne 0) {
        Err "failed to register frpc service"
        return
    }

    # Start the service
    & $FrpcBin start
    if ($LASTEXITCODE -ne 0) {
        Warn "frpc service registered but failed to start"
        Hint "Try: Start-Service frpc"
        return
    }
    Ok "frpc service installed and started"
}

# Prompt interactively for missing tunnel values.
# Sets script-scope $TenantName and $FrpsToken if not already set.
function Invoke-TunnelPrompts {
    $script:TenantPlaceholder = $false
    $script:TokenPlaceholder = $false

    if (-not $script:TenantName -or -not $script:FrpsToken) {
        Write-Host ""
        Write-Host "    Tunnel setup requires a tenant name, frps token, and SSH port."
        Write-Host "    If you don't have these yet, register at:"
        Write-Host "      https://$TunnelDomain"
        Write-Host "    You'll receive your setup command with all values pre-filled."
        Write-Host ""
    }

    if (-not $script:TenantName) {
        Write-Host "    Enter the tenant subdomain (e.g. 'alice' for alice.${TunnelDomain}):"
        Write-Host "    (press Enter to use placeholder — you can update later)"
        $userInput = Read-Host "    > "
        if ($userInput) {
            $script:TenantName = $userInput
        } else {
            # Derive from computer name
            $slug = $env:COMPUTERNAME.ToLower() -replace '[^a-z0-9-]', '-' -replace '^-+|-+$', '' -replace '-{2,}', '-'
            if ($slug) {
                $script:TenantName = $slug
                $script:TenantPlaceholder = $true
                Warn "Using placeholder tenant: $slug"
            } else {
                Write-Host "    Could not derive tenant from computer name. Please enter one:"
                $script:TenantName = Read-Host "    > "
                if (-not $script:TenantName) { Err "Tenant name is required" }
            }
        }
    }

    if (-not $script:FrpsToken) {
        Write-Host ""
        Write-Host "    Enter the frps auth token (press Enter to use placeholder):"
        $userInput = Read-Host "    > "
        if ($userInput) {
            $script:FrpsToken = $userInput
        } else {
            $script:FrpsToken = "CHANGE_ME"
            $script:TokenPlaceholder = $true
            Warn "Using placeholder token — frpc will not connect until updated"
        }
    }

    Validate-Inputs

    # Display config summary
    Write-Host ""
    Write-Host "    Tunnel configuration:"
    Write-Host "      Tenant:       $($script:TenantName).$TunnelDomain"
    Write-Host "      frps server:  ${FrpsServer}:7000"
    if ($script:TokenPlaceholder) {
        Write-Host "      frps token:   CHANGE_ME (placeholder)"
    } else {
        Write-Host "      frps token:   $($script:FrpsToken.Substring(0, [Math]::Min(8, $script:FrpsToken.Length)))..."
    }
    Write-Host "      SSH port:     $SshPort"
    Write-Host "      Local port:   $Port"

    if ($script:TenantPlaceholder -or $script:TokenPlaceholder) {
        Write-Host ""
        Write-Host "    frpc will be installed with placeholders. Update the config later:"
        Write-Host "      notepad $FrpcConfig"
        Write-Host "    Then restart frpc:"
        Write-Host "      & `"$FrpcBin`" stop; & `"$FrpcBin`" start"
        Write-Host ""
        Write-Host "    Or re-run: .\install.ps1 -Tunnel -TenantName <name> -FrpsToken <token>"
    }

    Write-Host ""
    Read-Host "    Press Enter to continue, or Ctrl+C to abort"
}

# ── Validate inputs ──────────────────────────────────────────────────
Validate-Inputs

# ══════════════════════════════════════════════════════════════════════
# ── Tunnel-only update (when octos is already installed) ─────────────
# ══════════════════════════════════════════════════════════════════════
# If octos binary exists and user passed -TenantName or -FrpsToken,
# skip the full install and just update the tunnel configuration.

$octosBinCheck = Join-Path $Prefix "octos.exe"
if ((Test-Path $octosBinCheck) -and ($TenantName -or $FrpsToken)) {
    Section "Updating tunnel configuration"

    # Fill in missing values from existing frpc config
    if (Test-Path $FrpcConfig) {
        $existingConfig = Get-Content $FrpcConfig -Raw -ErrorAction SilentlyContinue
        if (-not $TenantName -and $existingConfig -match 'customDomains\s*=\s*\["([^.]+)\.') {
            $TenantName = $Matches[1]
            Ok "tenant name from existing config: $TenantName"
        }
        if (-not $FrpsToken -and $existingConfig -match 'auth\.token\s*=\s*"([^"]+)"') {
            $FrpsToken = $Matches[1]
            Ok "frps token from existing config: $($FrpsToken.Substring(0, [Math]::Min(8, $FrpsToken.Length)))..."
        }
        if ($SshPort -eq 6001 -and $existingConfig -match 'remotePort\s*=\s*(\d+)') {
            $existingSshPort = [int]$Matches[1]
            if ($existingSshPort -ne 6001) {
                $SshPort = $existingSshPort
                Ok "ssh port from existing config: $SshPort"
            }
        }
    }

    # Prompt for anything still missing
    Invoke-TunnelPrompts

    # Install frpc if missing
    Install-FrpcBinary

    # Write config
    Write-FrpcConfig
    Ok "frpc config updated"

    # Restart service
    & $FrpcBin stop 2>$null
    & $FrpcBin uninstall 2>$null
    Start-Sleep -Seconds 1
    & $FrpcBin install -c $FrpcConfig 2>$null
    & $FrpcBin start 2>$null
    Ok "frpc restarted"

    # Verify
    Start-Sleep -Seconds 2
    $svc = Get-Service frpc -ErrorAction SilentlyContinue
    if ($svc -and $svc.Status -eq "Running") {
        Ok "frpc is running"
    } else {
        Warn "frpc does not appear to be running"
        Write-Host "    Check logs: Get-Content '$FrpcLog' -Tail 20"
    }

    Write-Host ""
    Write-Host "    Tunnel: https://${TenantName}.${TunnelDomain}"
    Write-Host ""
    exit 0
}

# ══════════════════════════════════════════════════════════════════════
# ── Doctor mode ──────────────────────────────────────────────────────
# ══════════════════════════════════════════════════════════════════════
if ($Doctor) {
    $script:DoctorIssues = 0

    # Auto-detect port from installed service config (unless user passed -Port)
    if ($Port -eq 8080) {
        $wrapperPath = Join-Path $DataDir "serve-launcher.cmd"
        if (Test-Path $wrapperPath) {
            $wrapperContent = Get-Content $wrapperPath -Raw -ErrorAction SilentlyContinue
            if ($wrapperContent -match '--port\s+(\d+)') {
                $Port = [int]$Matches[1]
            }
        }
    }

    # ── Binary ───────────────────────────────────────────────────────
    Section "octos binary"

    $OctosBin = Join-Path $Prefix "octos.exe"
    if (Test-Path $OctosBin) {
        Ok "found: $OctosBin"
        try {
            $ver = & $OctosBin --version 2>&1 | Select-Object -First 1
            Ok "version: $ver"
        } catch {
            Err "binary exists but failed to run"
            Hint "Try reinstalling: irm https://github.com/octos-org/octos/releases/latest/download/install.ps1 | iex"
        }
    } else {
        if (Test-Command "octos") {
            $found = (Get-Command octos).Source
            Warn "not found at $OctosBin, but found at $found"
            Hint "Set `$env:OCTOS_PREFIX or check your PATH"
        } else {
            Err "octos binary not found"
            Hint "Run install.ps1 to install"
        }
    }

    # ── Data directory ───────────────────────────────────────────────
    Section "Data directory"

    if (Test-Path $DataDir) {
        Ok "found: $DataDir"
        $configPath = Join-Path $DataDir "config.json"
        if (Test-Path $configPath) {
            Ok "config.json exists"
        } else {
            Warn "config.json missing"
            Hint "Run: octos init"
        }
    } else {
        Err "$DataDir does not exist"
        Hint "Run: octos init --defaults"
    }

    # ── octos serve process ──────────────────────────────────────────
    Section "octos serve"

    $octosProc = Get-Process -Name "octos" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -match "serve" } |
        Select-Object -First 1
    if ($octosProc) {
        Ok "running (PID: $($octosProc.Id))"
    } else {
        Err "octos serve is not running"
        Hint "Start: Start-ScheduledTask -TaskName OctosServe"
    }

    # ── Port check ───────────────────────────────────────────────────
    Section "Port $Port"

    $listener = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($listener) {
        $proc = Get-Process -Id $listener.OwningProcess -ErrorAction SilentlyContinue
        if ($proc -and $proc.ProcessName -match "octos") {
            Ok "port $Port held by octos (PID: $($proc.Id))"
        } elseif ($proc) {
            Err "port $Port held by $($proc.ProcessName) (PID: $($proc.Id)) - not octos"
            Hint "Stop it: Stop-Process -Id $($proc.Id)"
        } else {
            Warn "port $Port in use but owning process not found"
        }
    } else {
        if ($octosProc) {
            Err "octos serve is running but nothing is listening on $Port"
        } else {
            Warn "nothing listening on port $Port"
        }
    }

    # ── Admin portal ─────────────────────────────────────────────────
    Section "Admin portal"

    try {
        $resp = Invoke-WebRequest -Uri "http://localhost:${Port}/admin/" -UseBasicParsing -TimeoutSec 3 -ErrorAction Stop
        if ($resp.StatusCode -eq 200) {
            Ok "http://localhost:${Port}/admin/ responds 200"
        }
    } catch {
        $status = 0
        if ($_.Exception.Response) {
            $status = [int]$_.Exception.Response.StatusCode
        }
        switch ($status) {
            401 { Warn "responds 401 (auth required)"; Hint "Pass auth token in request header" }
            403 { Warn "responds 403 (forbidden)"; Hint "Check auth configuration" }
            404 { Err "responds 404 (admin route not found)"; Hint "Binary may be built without 'api' feature. Rebuild with: cargo build --features api" }
            default { Err "connection failed (server not reachable on localhost:${Port})"; Hint "Check 'octos serve' section above" }
        }
    }

    # ── PATH ─────────────────────────────────────────────────────────
    Section "PATH configuration"

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($userPath -split ";" | Where-Object { $_ -eq $Prefix }) {
        Ok "$Prefix is in User PATH"
    } else {
        Warn "$Prefix is not in User PATH"
        Hint "Add it: [Environment]::SetEnvironmentVariable('Path', '$Prefix;' + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')"
    }

    # ── Runtime dependencies ─────────────────────────────────────────
    Section "Runtime dependencies"

    if (Test-Command "git") {
        $gitVer = git --version 2>&1
        Ok "git $gitVer"
    } else { Warn "git not found"; Hint (Get-PkgHint "git") }

    if (Test-Command "node") {
        $nodeVer = node --version 2>&1
        Ok "Node.js $nodeVer"
    } else { Warn "Node.js not found (optional)"; Hint (Get-PkgHint "node") }

    if (Test-Command "python") {
        $pyVer = python --version 2>&1
        Ok "Python $pyVer"
    } else { Warn "Python not found"; Hint (Get-PkgHint "python") }

    if (Test-Command "ffmpeg") {
        Ok "ffmpeg found"
    } else { Warn "ffmpeg not found (optional)"; Hint (Get-PkgHint "ffmpeg") }

    if (Test-Command "caddy") {
        Ok "Caddy: $(caddy version 2>&1)"
    } else { Warn "Caddy not found (needed for HTTPS)" }

    $chromeFound = $false
    $chromePaths = @(
        "${env:ProgramFiles}\Google\Chrome\Application\chrome.exe",
        "${env:ProgramFiles(x86)}\Google\Chrome\Application\chrome.exe",
        "$env:LOCALAPPDATA\Google\Chrome\Application\chrome.exe"
    )
    foreach ($p in $chromePaths) {
        if (Test-Path $p) {
            Ok "Browser: $p"
            $chromeFound = $true
            break
        }
    }
    if (-not $chromeFound) {
        if (Test-Command "chrome") { Ok "Browser: chrome (in PATH)"; $chromeFound = $true }
        elseif (Test-Command "chromium") { Ok "Browser: chromium (in PATH)"; $chromeFound = $true }
    }
    if (-not $chromeFound) { Warn "Chrome/Chromium not found (optional)"; Hint (Get-PkgHint "chromium") }

    # ── Service configuration ────────────────────────────────────────
    Section "Service configuration"

    $task = Get-ScheduledTask -TaskName "OctosServe" -ErrorAction SilentlyContinue
    if ($task) {
        Ok "OctosServe task registered"
        $taskInfo = $task | Get-ScheduledTaskInfo -ErrorAction SilentlyContinue
        if ($taskInfo -and $taskInfo.LastRunTime) {
            Ok "last run: $($taskInfo.LastRunTime)"
        }
        if ($taskInfo -and $taskInfo.LastTaskResult -ne 0 -and $taskInfo.LastTaskResult -ne 267009) {
            Warn "last result code: $($taskInfo.LastTaskResult)"
        }
        # Check wrapper script
        $wrapperPath = Join-Path $DataDir "serve-launcher.cmd"
        if (Test-Path $wrapperPath) {
            Ok "launcher: $wrapperPath"
        } else {
            Err "serve-launcher.cmd missing"
            Hint "Re-run install.ps1 to recreate it"
        }
    } else {
        Err "OctosServe scheduled task not found"
        Hint "Re-run install.ps1 to create it"
    }

    # ── Recent serve logs ────────────────────────────────────────────
    Section "Recent serve logs"

    $serveLog = Join-Path $DataDir "serve.log"
    if (Test-Path $serveLog) {
        $recentErrors = Get-Content $serveLog -Tail 50 -ErrorAction SilentlyContinue |
            Select-String -Pattern "ERROR|panic|FATAL" -SimpleMatch |
            Select-Object -Last 5
        if ($recentErrors) {
            Warn "recent errors in serve.log:"
            $recentErrors | ForEach-Object { Write-Host "      $_" -ForegroundColor Yellow }
        } else {
            Ok "no recent errors in serve.log"
        }
    } else {
        Warn "serve.log not found at $serveLog"
        Hint "octos serve may not have started yet"
    }

    # ── frpc tunnel (only if tunnel was ever configured) ─────────────
    $FrpcBinDoc = Join-Path $Prefix "frpc.exe"
    $FrpcConfigDoc = Join-Path $DataDir "frpc.toml"
    $FrpcLogDoc = Join-Path $DataDir "logs\frpc.log"

    if ((Test-Path $FrpcBinDoc) -or (Test-Path $FrpcConfigDoc)) {
        Section "frpc tunnel"

        $TenantDoc = ""

        if (Test-Path $FrpcBinDoc) {
            try {
                $frpcVer = & $FrpcBinDoc --version 2>&1 | Select-Object -First 1
                Ok "frpc installed: $frpcVer"
            } catch {
                Ok "frpc installed (version unknown)"
            }
        } else {
            Warn "frpc binary not found"
            Hint "Re-run install.ps1 with -Tunnel -TenantName <name> -FrpsToken <token>"
        }

        # frpc service
        $frpcSvc = Get-Service frpc -ErrorAction SilentlyContinue
        if ($frpcSvc) {
            if ($frpcSvc.Status -eq "Running") {
                Ok "frpc service running"
            } else {
                Err "frpc service registered but not running (status: $($frpcSvc.Status))"
                Hint "Start-Service frpc"
            }
        } else {
            if (Test-Path $FrpcBinDoc) {
                Err "frpc installed but service not registered"
                Hint "Re-run install.ps1 with -Tunnel to register the service"
            }
        }

        # frpc config
        if (Test-Path $FrpcConfigDoc) {
            Ok "frpc config: $FrpcConfigDoc"
            $configContent = Get-Content $FrpcConfigDoc -Raw -ErrorAction SilentlyContinue
            if ($configContent -match 'customDomains\s*=\s*\["([^"]+)"\]') {
                $TenantDoc = $Matches[1]
                Write-Host "    Tunnel: https://$TenantDoc"
            }
            if ($configContent -match 'CHANGE_ME') {
                Warn "frpc config contains placeholder token (CHANGE_ME)"
                Hint "Update: notepad $FrpcConfigDoc"
                Hint "Or re-run: .\install.ps1 -Tunnel -TenantName <name> -FrpsToken <token>"
            }
        } elseif (Test-Path $FrpcBinDoc) {
            Warn "frpc installed but no config at $FrpcConfigDoc"
            Hint "Re-run install.ps1 with -Tunnel -TenantName <name> -FrpsToken <token>"
        }

        # frpc logs
        if (Test-Path $FrpcLogDoc) {
            $frpcErrors = Get-Content $FrpcLogDoc -Tail 20 -ErrorAction SilentlyContinue |
                Select-String -Pattern "error|failed|refused" |
                Select-Object -Last 3
            if ($frpcErrors) {
                Warn "recent frpc errors:"
                $frpcErrors | ForEach-Object { Write-Host "      $_" -ForegroundColor Yellow }
                Hint "Full log: Get-Content '$FrpcLogDoc' -Tail 50"
            }
        }

        # ── Remote access ───────────────────────────────────────────
        Section "Remote access"

        $adminOk = $false
        try {
            $resp = Invoke-WebRequest -Uri "http://localhost:${Port}/admin/" -UseBasicParsing -TimeoutSec 3 -ErrorAction Stop
            if ($resp.StatusCode -eq 200) { $adminOk = $true }
        } catch {}

        $frpcOk = ($frpcSvc -and $frpcSvc.Status -eq "Running")

        if ($adminOk -and $frpcOk) {
            Ok "admin portal works locally and frpc tunnel is running"
            if ($TenantDoc) {
                Write-Host "    Remote URL: https://$TenantDoc"
            }
        } elseif ($adminOk -and -not $frpcOk) {
            if (-not (Test-Path $FrpcBinDoc)) {
                Err "admin portal works locally but frpc binary is missing"
                Hint "Re-run: .\install.ps1 -Tunnel -TenantName <name> -FrpsToken <token>"
            } elseif (-not $frpcSvc) {
                Err "admin portal works locally but frpc service is NOT registered"
                Hint "Re-run: .\install.ps1 -Tunnel -TenantName <name> -FrpsToken <token>"
            } else {
                Err "admin portal works locally but frpc is NOT running — remote access is down"
                Hint "Start-Service frpc"
            }
        } elseif (-not $adminOk) {
            Err "admin portal is not responding locally — fix octos serve first (see above)"
            Hint "Remote access depends on the local server working first"
        }
    }

    # ── Summary ──────────────────────────────────────────────────────
    Section "Summary"
    if ($script:DoctorIssues -eq 0) {
        Write-Host "    All checks passed. Everything looks healthy."
    } else {
        Write-Host "    Found $($script:DoctorIssues) issue(s). Review the hints above to fix them."
    }
    Write-Host ""
    exit 0
}

# ══════════════════════════════════════════════════════════════════════
# ── Uninstall mode ───────────────────────────────────────────────────
# ══════════════════════════════════════════════════════════════════════
if ($Uninstall) {
    Section "Uninstalling octos"

    # Remove frpc service and binary
    $FrpcBinUn = Join-Path $Prefix "frpc.exe"
    $FrpcConfigUn = Join-Path $DataDir "frpc.toml"
    $FrpcLogUn = Join-Path $DataDir "logs\frpc.log"
    if (Test-Path $FrpcBinUn) {
        & $FrpcBinUn uninstall 2>$null
        Ok "removed frpc service"
    }
    if (Test-Path $FrpcConfigUn) {
        Remove-Item $FrpcConfigUn -Force -ErrorAction SilentlyContinue
        Ok "removed $FrpcConfigUn"
    }
    if (Test-Path $FrpcLogUn) {
        Remove-Item $FrpcLogUn -Force -ErrorAction SilentlyContinue
        Ok "removed frpc log"
    }

    # Remove scheduled tasks
    $task = Get-ScheduledTask -TaskName "OctosServe" -ErrorAction SilentlyContinue
    if ($task) {
        Unregister-ScheduledTask -TaskName "OctosServe" -Confirm:$false -ErrorAction SilentlyContinue
        Ok "removed OctosServe scheduled task"
    }
    $caddyTask = Get-ScheduledTask -TaskName "OctosCaddy" -ErrorAction SilentlyContinue
    if ($caddyTask) {
        Unregister-ScheduledTask -TaskName "OctosCaddy" -Confirm:$false -ErrorAction SilentlyContinue
        Ok "removed OctosCaddy scheduled task"
    }

    # Stop octos and caddy processes
    Get-Process -Name "octos" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Ok "stopped octos processes"
    Get-Process -Name "caddy" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

    # Remove firewall rules
    netsh advfirewall firewall delete rule name="octos-serve" >$null 2>&1
    $fwServeExit = $LASTEXITCODE
    netsh advfirewall firewall delete rule name="octos-caddy" >$null 2>&1
    $fwCaddyExit = $LASTEXITCODE
    if ($fwServeExit -eq 0 -or $fwCaddyExit -eq 0) {
        Ok "removed firewall rules"
    } else {
        Warn "failed to remove some firewall rules (may require Administrator privileges)"
    }

    # Remove Caddyfile
    $caddyfileUn = Join-Path $DataDir "Caddyfile"
    if (Test-Path $caddyfileUn) {
        Remove-Item $caddyfileUn -Force -ErrorAction SilentlyContinue
        Ok "removed $caddyfileUn"
    }

    # Remove binaries
    if (Test-Path $Prefix) {
        Remove-Item -Recurse -Force $Prefix
        Ok "removed $Prefix"
    } else {
        Warn "$Prefix not found (already removed?)"
    }

    # Remove from User PATH
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($userPath) {
        $newPath = ($userPath -split ";" | Where-Object { $_ -ne $Prefix -and $_ -ne "" }) -join ";"
        if ($newPath -ne $userPath) {
            [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
            Ok "removed $Prefix from User PATH"
        }
    }

    Write-Host ""
    Write-Host "    Data directory ($DataDir) was NOT removed. Delete manually if desired:"
    Write-Host "      Remove-Item -Recurse -Force '$DataDir'"
    exit 0
}

# ══════════════════════════════════════════════════════════════════════
# ── Install mode (default) ───────────────────────────────────────────
# ══════════════════════════════════════════════════════════════════════

# ── Detect platform ──────────────────────────────────────────────────
Section "Detecting platform"

$arch = Get-WindowsArchitecture
switch ($arch) {
    "AMD64" {
        $Triple = "x86_64-pc-windows-msvc"
        Ok "Windows x64 ($Triple)"
    }
    "X64" {
        $Triple = "x86_64-pc-windows-msvc"
        Ok "Windows x64 ($Triple)"
    }
    "ARM64" {
        # ARM64 Windows can run x64 binaries via emulation
        $Triple = "x86_64-pc-windows-msvc"
        Warn "Windows ARM64 detected - using x64 binary (runs via emulation)"
    }
    default {
        Err "Unsupported architecture: $arch"
    }
}

# ── Auto-install helper ──────────────────────────────────────────────
function Install-Dep($name, $testCmd, $installBlock) {
    if (Test-Command $testCmd) { return $true }
    if (-not $InstallDeps) { return $false }
    Write-Host "    Installing $name..."
    try { & $installBlock; return $true } catch { Warn "Failed to install ${name}: $_"; return $false }
}

# ── Check/install runtime dependencies ───────────────────────────────
Section "Runtime dependencies$(if ($InstallDeps) { ' (auto-install)' })"

# git
if (Test-Command "git") {
    Ok "git $((git --version 2>&1) -replace 'git version ','')"
} elseif ($InstallDeps) {
    Install-Dep "Git" "git" {
        $url = "https://github.com/git-for-windows/git/releases/download/v2.53.0.windows.2/Git-2.53.0.2-64-bit.exe"
        $installer = Join-Path $env:TEMP "git-install.exe"
        (New-Object System.Net.WebClient).DownloadFile($url, $installer)
        Start-Process -Wait -FilePath $installer -ArgumentList "/VERYSILENT","/NORESTART","/SP-"
        $env:PATH = "C:\Program Files\Git\bin;$env:PATH"
    }
    if (Test-Command "git") { Ok "git installed" } else { Warn "git install failed" }
} else {
    Warn "git not found"; Hint (Get-PkgHint "git")
}

# Node.js
if (Test-Command "node") {
    Ok "Node.js $(node --version 2>&1)"
} elseif ($InstallDeps) {
    Install-Dep "Node.js" "node" {
        $url = "https://nodejs.org/dist/v22.15.0/node-v22.15.0-x64.msi"
        $installer = Join-Path $env:TEMP "node-install.msi"
        (New-Object System.Net.WebClient).DownloadFile($url, $installer)
        Start-Process -Wait msiexec -ArgumentList "/i","$installer","/qn","/norestart"
        $env:PATH = "C:\Program Files\nodejs;$env:PATH"
    }
    if (Test-Command "node") { Ok "Node.js installed" } else { Warn "Node.js install failed" }
} else {
    Warn "Node.js not found (optional)"; Hint (Get-PkgHint "node")
}

# ffmpeg
if (Test-Command "ffmpeg") {
    Ok "ffmpeg found"
} elseif ($InstallDeps) {
    Install-Dep "ffmpeg" "ffmpeg" {
        $url = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip"
        $zip = Join-Path $env:TEMP "ffmpeg.zip"
        (New-Object System.Net.WebClient).DownloadFile($url, $zip)
        Expand-Archive -Path $zip -DestinationPath $env:TEMP -Force
        $extracted = Get-ChildItem "$env:TEMP\ffmpeg-*-essentials_build" | Select-Object -First 1
        if ($extracted) {
            $ffDir = "C:\ffmpeg"
            New-Item -ItemType Directory -Path $ffDir -Force | Out-Null
            Copy-Item "$($extracted.FullName)\bin\*" $ffDir -Force
            $env:PATH = "$ffDir;$env:PATH"
            [Environment]::SetEnvironmentVariable("PATH", "$ffDir;$([Environment]::GetEnvironmentVariable('PATH','Machine'))", "Machine")
        }
    }
    if (Test-Command "ffmpeg") { Ok "ffmpeg installed" } else { Warn "ffmpeg install failed" }
} else {
    Warn "ffmpeg not found (optional)"; Hint (Get-PkgHint "ffmpeg")
}

# Python
if (Test-Command "python") {
    Ok "Python $(python --version 2>&1)"
} elseif ($InstallDeps) {
    Install-Dep "Python" "python" {
        $url = "https://www.python.org/ftp/python/3.13.5/python-3.13.5-amd64.exe"
        $installer = Join-Path $env:TEMP "python-install.exe"
        (New-Object System.Net.WebClient).DownloadFile($url, $installer)
        Start-Process -Wait -FilePath $installer -ArgumentList "/quiet","InstallAllUsers=1","PrependPath=1"
        # Refresh PATH so Test-Command can find the newly installed binary
        $env:PATH = [Environment]::GetEnvironmentVariable("PATH", "Machine") + ";" + [Environment]::GetEnvironmentVariable("PATH", "User")
    }
    if (Test-Command "python") { Ok "Python installed" } else { Warn "Python install failed" }
} else {
    Warn "Python not found"; Hint (Get-PkgHint "python")
}

# Chrome
$chromeFound = $false
$chromePaths = @(
    "${env:ProgramFiles}\Google\Chrome\Application\chrome.exe",
    "${env:ProgramFiles(x86)}\Google\Chrome\Application\chrome.exe",
    "$env:LOCALAPPDATA\Google\Chrome\Application\chrome.exe"
)
foreach ($p in $chromePaths) {
    if (Test-Path $p) { Ok "Browser: Chrome"; $chromeFound = $true; break }
}
if (-not $chromeFound -and $InstallDeps) {
    Write-Host "    Installing Chrome..."
    try {
        $url = "https://dl.google.com/chrome/install/latest/chrome_installer.exe"
        $installer = Join-Path $env:TEMP "chrome-install.exe"
        (New-Object System.Net.WebClient).DownloadFile($url, $installer)
        Start-Process -Wait -FilePath $installer -ArgumentList "/silent","/install"
        $chromeFound = $true; Ok "Chrome installed"
    } catch { Warn "Chrome install failed" }
} elseif (-not $chromeFound) {
    Warn "Chrome not found (optional)"; Hint (Get-PkgHint "chromium")
}

# Caddy (for HTTPS)
if (Test-Command "caddy") {
    Ok "Caddy: $(caddy version 2>&1)"
} elseif ($Domain -or $InstallDeps) {
    Write-Host "    Installing Caddy..."
    try {
        $url = "https://github.com/caddyserver/caddy/releases/download/v2.11.1/caddy_2.11.1_windows_amd64.zip"
        $zip = Join-Path $env:TEMP "caddy.zip"
        $caddyDir = "C:\caddy"
        (New-Object System.Net.WebClient).DownloadFile($url, $zip)
        New-Item -ItemType Directory -Path $caddyDir -Force | Out-Null
        Expand-Archive -Path $zip -DestinationPath $caddyDir -Force
        $env:PATH = "$caddyDir;$env:PATH"
        [Environment]::SetEnvironmentVariable("PATH", "$caddyDir;$([Environment]::GetEnvironmentVariable('PATH','Machine'))", "Machine")
        Ok "Caddy installed to $caddyDir"
    } catch { Warn "Caddy install failed: $_" }
} elseif (-not $Domain) {
    # Only warn if domain is requested
} else {
    Warn "Caddy not found (needed for HTTPS with -Domain)"
}

# ── Resolve download source ──────────────────────────────────────────
Section "Resolving release"

$Zipfile = "octos-bundle-${Triple}.zip"
$DownloadBase = $env:OCTOS_DOWNLOAD_URL

# Auto-detect: check if zip is next to the script or in the current directory
if (-not $DownloadBase) {
    $scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { $PWD.Path }
    if (Test-Path (Join-Path $scriptDir $Zipfile)) {
        $DownloadBase = $scriptDir
    } elseif (Test-Path (Join-Path $PWD.Path $Zipfile)) {
        $DownloadBase = $PWD.Path
    }
}

if ($DownloadBase) {
    # Local file or self-hosted
    $localPath = Join-Path $DownloadBase $Zipfile
    if (-not (Test-Path $localPath)) {
        Err "File not found: $localPath"
    }
    Ok "source: $localPath"
    $DownloadUrl = $null
} else {
    # GitHub Releases
    if ($Version -eq "latest") {
        try {
            $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$GithubRepo/releases/latest" -UseBasicParsing
            $Version = $release.tag_name
        } catch {
            Err "Could not determine latest release. Specify -Version explicitly."
        }
    }
    $DownloadUrl = "https://github.com/$GithubRepo/releases/download/$Version/$Zipfile"
    Ok "version: $Version"
}

# ── Download and install octos ────────────────────────────────────────
Section "Installing octos"

$installTmp = Join-Path ([System.IO.Path]::GetTempPath()) "octos-install-$([System.Guid]::NewGuid().ToString('N').Substring(0,8))"
New-Item -ItemType Directory -Path $installTmp -Force | Out-Null

try {
    $zipPath = Join-Path $installTmp $Zipfile

    if ($DownloadUrl) {
        Write-Host "    Downloading $Zipfile..."
        try {
            Invoke-WebRequest -Uri $DownloadUrl -OutFile $zipPath -UseBasicParsing
        } catch {
            Err "Download failed. Check that release $Version has a binary for $Triple."
        }
    } else {
        Write-Host "    Copying from $localPath..."
        Copy-Item $localPath $zipPath
    }

    # Extract
    $extractDir = Join-Path $installTmp "extracted"
    Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force

    # Install binaries
    if (-not (Test-Path $Prefix)) {
        New-Item -ItemType Directory -Path $Prefix -Force | Out-Null
    }

    $binCount = 0
    Get-ChildItem -Path $extractDir -File | ForEach-Object {
        Copy-Item $_.FullName -Destination $Prefix -Force
        $binCount++
    }
    # Also check for nested directory (some zip layouts)
    if ($binCount -eq 0) {
        Get-ChildItem -Path $extractDir -Directory | ForEach-Object {
            Get-ChildItem -Path $_.FullName -File | ForEach-Object {
                Copy-Item $_.FullName -Destination $Prefix -Force
                $binCount++
            }
        }
    }
    Ok "$binCount binaries installed to $Prefix"

} finally {
    Remove-Item -Recurse -Force $installTmp -ErrorAction SilentlyContinue
}

# ── Add to PATH ───────────────────────────────────────────────────────
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not ($userPath -split ";" | Where-Object { $_ -eq $Prefix })) {
    [Environment]::SetEnvironmentVariable("Path", "$Prefix;$userPath", "User")
    # Also update current session
    $env:Path = "$Prefix;$env:Path"
    Ok "added $Prefix to User PATH"
    Warn "Restart your terminal for PATH changes to take effect in new sessions"
} else {
    Ok "$Prefix already in PATH"
}

# ── Initialize octos workspace ────────────────────────────────────────
Section "Initializing octos"

$env:OCTOS_HOME = $DataDir
$octosBin = Join-Path $Prefix "octos.exe"

if (-not (Test-Path $DataDir)) {
    if ($DataDir -eq (Join-Path $HOME ".octos")) {
        try {
            & $octosBin init --cwd $HOME --defaults 2>&1 | Out-Null
            Ok "workspace initialized via octos init"
        } catch {
            try {
                & $octosBin init --cwd $HOME 2>&1 | Out-Null
                Ok "workspace initialized via octos init"
            } catch {
                New-Item -ItemType Directory -Path $DataDir -Force | Out-Null
                Ok "created data directory: $DataDir"
            }
        }
    } else {
        New-Item -ItemType Directory -Path $DataDir -Force | Out-Null
        Ok "created custom data directory: $DataDir"
    }
} else {
    Ok "$DataDir already exists (skipping init)"
}

# Ensure required subdirectories exist
$subdirs = @("profiles", "memory", "sessions", "skills", "logs", "research", "history")
foreach ($d in $subdirs) {
    $dirPath = Join-Path $DataDir $d
    if (-not (Test-Path $dirPath)) {
        New-Item -ItemType Directory -Path $dirPath -Force | Out-Null
    }
}

# Bootstrap config.json — auto-detect provider from available API keys
$configPath = Join-Path $DataDir "config.json"
if (-not (Test-Path $configPath)) {
    if ($env:OPENAI_API_KEY)     { $_prov = "openai";    $_model = "gpt-4.1-mini";            $_env = "OPENAI_API_KEY" }
    elseif ($env:ANTHROPIC_API_KEY) { $_prov = "anthropic"; $_model = "claude-sonnet-4-20250514"; $_env = "ANTHROPIC_API_KEY" }
    elseif ($env:GEMINI_API_KEY) { $_prov = "gemini";    $_model = "gemini-2.5-flash";         $_env = "GEMINI_API_KEY" }
    elseif ($env:DEEPSEEK_API_KEY) { $_prov = "deepseek"; $_model = "deepseek-chat";            $_env = "DEEPSEEK_API_KEY" }
    elseif ($env:KIMI_API_KEY)   { $_prov = "moonshot";  $_model = "kimi-k2.5";                $_env = "KIMI_API_KEY" }
    elseif ($env:DASHSCOPE_API_KEY) { $_prov = "dashscope"; $_model = "qwen3.5-plus";           $_env = "DASHSCOPE_API_KEY" }
    else                         { $_prov = "openai";    $_model = "gpt-4.1-mini";            $_env = "OPENAI_API_KEY" }

    $configJson = @"
{
  "provider": "$_prov",
  "model": "$_model",
  "api_key_env": "$_env"
}
"@
    # WriteAllText avoids UTF-8 BOM that PowerShell 5.1 -Encoding UTF8 adds
    [System.IO.File]::WriteAllText($configPath, $configJson, [System.Text.UTF8Encoding]::new($false))
    Ok "auto-detected provider: $_prov ($_env)"
} else {
    # Read api_key_env from existing config so the summary hint is correct
    try {
        $existingCfg = Get-Content $configPath -Raw -ErrorAction SilentlyContinue | ConvertFrom-Json
        $_env = $existingCfg.api_key_env
    } catch {}
}
if (-not $_env) { $_env = "OPENAI_API_KEY" }

# Bootstrap other files
$gitignorePath = Join-Path $DataDir ".gitignore"
if (-not (Test-Path $gitignorePath)) {
    @"
# Ignore task state and database files
tasks/
sessions/
*.redb
"@ | Set-Content -Path $gitignorePath -Encoding UTF8
}

$agentsPath = Join-Path $DataDir "AGENTS.md"
if (-not (Test-Path $agentsPath)) {
    "# Agent Instructions`n`nCustomize agent behavior and guidelines here.`n" | Set-Content -Path $agentsPath -Encoding UTF8
}

$soulPath = Join-Path $DataDir "SOUL.md"
if (-not (Test-Path $soulPath)) {
    "# Personality`n`nDefine the agent's personality and values.`n" | Set-Content -Path $soulPath -Encoding UTF8
}

$userPath2 = Join-Path $DataDir "USER.md"
if (-not (Test-Path $userPath2)) {
    "# User Info`n`nAdd your information and preferences here.`n" | Set-Content -Path $userPath2 -Encoding UTF8
}

Ok "data directory: $DataDir"

# ── Generate auth token ──────────────────────────────────────────────
if (-not $AuthToken) { $AuthToken = if ($env:OCTOS_AUTH_TOKEN) { $env:OCTOS_AUTH_TOKEN } else { "" } }
if (-not $AuthToken) {
    # Generate 32-byte hex token
    $bytes = New-Object byte[] 32
    ([System.Security.Cryptography.RandomNumberGenerator]::Create()).GetBytes($bytes)
    $AuthToken = ($bytes | ForEach-Object { $_.ToString("x2") }) -join ""
}

# ── Set up octos serve as scheduled task ─────────────────────────────
Section "Setting up octos serve"

$serveLog = Join-Path $DataDir "serve.log"
$taskName = "OctosServe"

# Remove existing task if present
$existingTask = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
if ($existingTask) {
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
}

# Stop any running octos serve processes before re-registering
Get-Process -Name "octos" -ErrorAction SilentlyContinue |
    Where-Object { $_.CommandLine -match "serve" } |
    Stop-Process -Force -ErrorAction SilentlyContinue

$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME

$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -StartWhenAvailable `
    -RestartCount 3 `
    -RestartInterval (New-TimeSpan -Minutes 1) `
    -ExecutionTimeLimit ([TimeSpan]::Zero)

# Build a wrapper script that sets env vars and launches octos serve
$wrapperPath = Join-Path $DataDir "serve-launcher.cmd"
$wrapperContent = @"
@echo off
set "OCTOS_HOME=$DataDir"
set "OCTOS_DATA_DIR=$DataDir"
set "OCTOS_AUTH_TOKEN=$AuthToken"
"$octosBin" serve --port $Port --host 0.0.0.0 --auth-token $AuthToken >> "$serveLog" 2>&1
"@
[System.IO.File]::WriteAllText($wrapperPath, $wrapperContent, [System.Text.UTF8Encoding]::new($false))

$action = New-ScheduledTaskAction `
    -Execute "cmd.exe" `
    -Argument "/C `"$wrapperPath`"" `
    -WorkingDirectory $HOME

Register-ScheduledTask `
    -TaskName $taskName `
    -Action $action `
    -Trigger $trigger `
    -Settings $settings `
    -Description "octos serve (dashboard + gateway)" `
    -RunLevel Limited `
    -Force | Out-Null

Ok "registered scheduled task: $taskName"

# Start the task now
Start-ScheduledTask -TaskName $taskName
Ok "octos serve starting"

# ── Verify octos serve ───────────────────────────────────────────────
Section "Verifying octos serve"

$retries = 10
while ($retries -gt 0) {
    try {
        $resp = Invoke-WebRequest -Uri "http://localhost:${Port}/admin/" -UseBasicParsing -TimeoutSec 2 -ErrorAction Stop
        if ($resp.StatusCode -eq 200) {
            Ok "octos serve is running on http://localhost:${Port}"
            break
        }
    } catch {}
    $retries--
    Start-Sleep -Seconds 1
}
if ($retries -eq 0) {
    Warn "octos serve did not respond within 10 seconds"
    Write-Host "    Check logs: Get-Content '$serveLog' -Tail 20"
}

# ── Firewall ─────────────────────────────────────────────────────────
Section "Configuring firewall"
netsh advfirewall firewall delete rule name="octos-serve" >$null 2>&1
netsh advfirewall firewall add rule name="octos-serve" dir=in action=allow protocol=TCP localport=$Port >$null 2>&1
if ($LASTEXITCODE -eq 0) {
    Ok "Firewall: port $Port open"
} else {
    Warn "Failed to configure firewall (requires elevated privileges)"
    Hint "Run as Administrator, or manually: netsh advfirewall firewall add rule name=`"octos-serve`" dir=in action=allow protocol=TCP localport=$Port"
}
if ($Domain) {
    netsh advfirewall firewall delete rule name="octos-caddy" >$null 2>&1
    netsh advfirewall firewall add rule name="octos-caddy" dir=in action=allow protocol=TCP localport=80,443 >$null 2>&1
    if ($LASTEXITCODE -eq 0) {
        Ok "Firewall: ports 80,443 open for Caddy"
    } else {
        Warn "Failed to open Caddy ports (requires elevated privileges)"
    }
}

# ── Caddy setup (HTTPS) ─────────────────────────────────────────────
if ($Domain -and (Test-Command "caddy")) {
    Section "Configuring Caddy for $Domain"
    $caddyfile = Join-Path $DataDir "Caddyfile"
    $caddyUpstream = "localhost:$Port"
    @"
{
    on_demand_tls {
        ask http://localhost:9999/check
    }
}

:9999 {
    respond /check 200
}

$Domain {
    handle /api/* {
        reverse_proxy $caddyUpstream
    }
    handle /admin* {
        reverse_proxy $caddyUpstream
    }
    handle /auth/* {
        reverse_proxy $caddyUpstream
    }
    handle /webhook/* {
        reverse_proxy $caddyUpstream
    }
    handle {
        reverse_proxy $caddyUpstream
    }
}

*.$Domain {
    tls {
        on_demand
    }

    @api path /api/*
    @admin path /admin*
    @auth path /auth/*

    handle @api {
        reverse_proxy $caddyUpstream {
            header_up X-Profile-Id {labels.2}
        }
    }
    handle @admin {
        reverse_proxy $caddyUpstream {
            header_up X-Profile-Id {labels.2}
        }
    }
    handle @auth {
        reverse_proxy $caddyUpstream {
            header_up X-Profile-Id {labels.2}
        }
    }
    handle {
        reverse_proxy $caddyUpstream
    }
}
"@ | Set-Content -Path $caddyfile -Encoding UTF8
    caddy fmt --overwrite $caddyfile 2>$null
    Ok "Caddyfile written to $caddyfile"

    # Validate
    $validateResult = caddy validate --config $caddyfile 2>&1
    if ($LASTEXITCODE -eq 0) {
        Ok "Caddyfile is valid"
    } else {
        Warn "Caddyfile validation failed — check $caddyfile"
    }

    # Register Caddy as scheduled task
    $caddyTask = "OctosCaddy"
    Unregister-ScheduledTask -TaskName $caddyTask -Confirm:$false -ErrorAction SilentlyContinue

    # Reload if already running, otherwise start via scheduled task
    $caddyProc = Get-Process -Name "caddy" -ErrorAction SilentlyContinue
    if ($caddyProc) {
        caddy reload --config $caddyfile 2>$null
        Ok "Caddy reloaded"
    }

    $caddyAction = New-ScheduledTaskAction -Execute "caddy" -Argument "run --config `"$caddyfile`""
    $caddyTrigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
    Register-ScheduledTask -TaskName $caddyTask -Action $caddyAction -Trigger $caddyTrigger -Settings $settings -Description "Caddy reverse proxy for octos" -RunLevel Limited -Force | Out-Null
    if (-not $caddyProc) {
        Start-ScheduledTask -TaskName $caddyTask
    }
    Ok "Caddy configured for $Domain (HTTPS auto-provisioned)"
}

# ── Tunnel setup (frpc) ──────────────────────────────────────────────
if ($Tunnel) {
    Section "Tunnel setup"

    # Prompt for missing inputs
    Invoke-TunnelPrompts

    # Install frpc binary
    Install-FrpcBinary

    # Write frpc config
    Write-FrpcConfig
    Ok "frpc config written to $FrpcConfig"

    # Register and start frpc service
    Write-Host "    (registering frpc as Windows Service)"
    Install-FrpcService

    # Verify tunnel
    Section "Verifying tunnel"
    Start-Sleep -Seconds 3
    $svc = Get-Service frpc -ErrorAction SilentlyContinue
    if ($svc -and $svc.Status -eq "Running") {
        Ok "frpc is running"
    } else {
        Warn "frpc does not appear to be running"
        Write-Host "    Check logs: Get-Content '$FrpcLog' -Tail 20"
    }

    try {
        $resp = Invoke-WebRequest -Uri "http://localhost:${Port}/api/status" -UseBasicParsing -TimeoutSec 3 -ErrorAction Stop
        Ok "octos serve is running on port ${Port}"
    } catch {
        Warn "octos serve is not responding on port ${Port} (tunnel will retry once it starts)"
    }
}

# ── Summary ───────────────────────────────────────────────────────────
Section "Installation complete!"
Write-Host ""
Write-Host "    Binary:     $octosBin"
Write-Host "    Data dir:   $DataDir"
Write-Host "    Config:     $configPath"
Write-Host "    Auth token: $AuthToken"
Write-Host "    Serve log:  $serveLog"
Write-Host ""
Write-Host "  Next steps:"
Write-Host "    1. Set your API key:  `$env:$_env = 'sk-...'"
Write-Host "    2. Install skills:    octos skills install --all"
Write-Host "    3. Start chatting:    octos chat"
Write-Host "    4. Open dashboard:    http://localhost:${Port}/admin/"
Write-Host ""
Write-Host "  Manage service:"
Write-Host "    Status:  Get-ScheduledTask -TaskName OctosServe"
Write-Host "    Stop:    Stop-ScheduledTask -TaskName OctosServe"
Write-Host "    Start:   Start-ScheduledTask -TaskName OctosServe"
Write-Host ""
if ($Tunnel) {
    Write-Host "  Tunnel:"
    Write-Host "    Dashboard:   https://${TenantName}.${TunnelDomain}"
    Write-Host "    frpc config: $FrpcConfig"
    Write-Host ""
    Write-Host "  Manage tunnel:"
    Write-Host "    Status:  Get-Service frpc"
    Write-Host "    Stop:    & `"$FrpcBin`" stop"
    Write-Host "    Start:   & `"$FrpcBin`" start"
    Write-Host ""
}
Write-Host "  Troubleshoot:"
Write-Host "    irm https://github.com/octos-org/octos/releases/latest/download/install.ps1 -OutFile install.ps1; .\install.ps1 -Doctor"
Write-Host ""
