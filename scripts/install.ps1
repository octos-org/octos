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

OPTIONS
  -Version <tag>     Release version (default: latest)
  -Prefix <path>     Install prefix (default: ~\.octos\bin)
  -AuthToken <token> Auth token for octos serve (default: auto-generated)
  -Doctor            Diagnose installation and service health
  -Uninstall         Remove octos binaries, scheduled task, and PATH entry
  -Help              Show this help message

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

# ── Helpers ───────────────────────────────────────────────────────────
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

# Print install hint for a package on Windows
function Get-PkgHint($pkg) {
    switch ($pkg) {
        "git"      { "winget install Git.Git" }
        "node"     { "winget install OpenJS.NodeJS.LTS" }
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

# ══════════════════════════════════════════════════════════════════════
# ── Doctor mode ──────────────────────────────────────────────────────
# ══════════════════════════════════════════════════════════════════════
if ($Doctor) {
    $script:DoctorIssues = 0

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

    # ── Port 8080 ────────────────────────────────────────────────────
    Section "Port 8080"

    $listener = Get-NetTCPConnection -LocalPort 8080 -State Listen -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($listener) {
        $proc = Get-Process -Id $listener.OwningProcess -ErrorAction SilentlyContinue
        if ($proc -and $proc.ProcessName -match "octos") {
            Ok "port 8080 held by octos (PID: $($proc.Id))"
        } elseif ($proc) {
            Err "port 8080 held by $($proc.ProcessName) (PID: $($proc.Id)) - not octos"
            Hint "Stop it: Stop-Process -Id $($proc.Id)"
        } else {
            Warn "port 8080 in use but owning process not found"
        }
    } else {
        if ($octosProc) {
            Err "octos serve is running but nothing is listening on 8080"
        } else {
            Warn "nothing listening on port 8080"
        }
    }

    # ── Admin portal ─────────────────────────────────────────────────
    Section "Admin portal"

    try {
        $resp = Invoke-WebRequest -Uri "http://localhost:8080/admin/" -UseBasicParsing -TimeoutSec 3 -ErrorAction Stop
        if ($resp.StatusCode -eq 200) {
            Ok "http://localhost:8080/admin/ responds 200"
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
            default { Err "connection failed (server not reachable on localhost:8080)"; Hint "Check 'octos serve' section above" }
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

    if (Test-Command "ffmpeg") {
        Ok "ffmpeg found"
    } else { Warn "ffmpeg not found (optional)"; Hint (Get-PkgHint "ffmpeg") }

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

    # Remove scheduled task
    $task = Get-ScheduledTask -TaskName "OctosServe" -ErrorAction SilentlyContinue
    if ($task) {
        Unregister-ScheduledTask -TaskName "OctosServe" -Confirm:$false -ErrorAction SilentlyContinue
        Ok "removed OctosServe scheduled task"
    }

    # Stop octos processes
    Get-Process -Name "octos" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Ok "stopped octos processes"

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

# ── Check runtime dependencies ────────────────────────────────────────
Section "Checking runtime dependencies"

# git - needed for skill installation
if (Test-Command "git") {
    $gitVer = (git --version 2>&1) -replace "git version ",""
    Ok "git $gitVer"
} else {
    Warn "git not found"
    Write-Host "    Enables: skill installation (octos skills install)"
    Hint (Get-PkgHint "git")
}

# Node.js - optional
if (Test-Command "node") {
    Ok "Node.js $(node --version 2>&1)"
} else {
    Warn "Node.js not found"
    Write-Host "    Enables: WhatsApp bridge, custom skills with package.json, pptxgenjs"
    Hint (Get-PkgHint "node")
}

# Chrome - optional
$chromeFound = $false
$chromePaths = @(
    "${env:ProgramFiles}\Google\Chrome\Application\chrome.exe",
    "${env:ProgramFiles(x86)}\Google\Chrome\Application\chrome.exe",
    "$env:LOCALAPPDATA\Google\Chrome\Application\chrome.exe"
)
foreach ($p in $chromePaths) {
    if (Test-Path $p) {
        Ok "Browser: Chrome"
        $chromeFound = $true
        break
    }
}
if (-not $chromeFound) {
    Warn "Chrome not found"
    Write-Host "    Enables: browser tool (web browsing, screenshots), deep-crawl skill"
    Hint (Get-PkgHint "chromium")
}

# ffmpeg - optional
if (Test-Command "ffmpeg") {
    Ok "ffmpeg found"
} else {
    Warn "ffmpeg not found"
    Write-Host "    Enables: voice/audio skills, media transcoding"
    Hint (Get-PkgHint "ffmpeg")
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

# Bootstrap config.json
$configPath = Join-Path $DataDir "config.json"
if (-not (Test-Path $configPath)) {
    $configJson = @'
{
  "provider": "anthropic",
  "model": "claude-sonnet-4-20250514",
  "api_key_env": "ANTHROPIC_API_KEY"
}
'@
    # WriteAllText avoids UTF-8 BOM that PowerShell 5.1 -Encoding UTF8 adds
    [System.IO.File]::WriteAllText($configPath, $configJson, [System.Text.UTF8Encoding]::new($false))
}

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
"$octosBin" serve --port 8080 --auth-token $AuthToken >> "$serveLog" 2>&1
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
        $resp = Invoke-WebRequest -Uri "http://localhost:8080/admin/" -UseBasicParsing -TimeoutSec 2 -ErrorAction Stop
        if ($resp.StatusCode -eq 200) {
            Ok "octos serve is running on http://localhost:8080"
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
Write-Host "    1. Set your API key:  `$env:ANTHROPIC_API_KEY = 'sk-...'"
Write-Host "    2. Install skills:    octos skills install --all"
Write-Host "    3. Start chatting:    octos chat"
Write-Host "    4. Open dashboard:    http://localhost:8080/admin/"
Write-Host ""
Write-Host "  Manage service:"
Write-Host "    Status:  Get-ScheduledTask -TaskName OctosServe"
Write-Host "    Stop:    Stop-ScheduledTask -TaskName OctosServe"
Write-Host "    Start:   Start-ScheduledTask -TaskName OctosServe"
Write-Host ""
Write-Host "  Troubleshoot:"
Write-Host "    irm https://github.com/octos-org/octos/releases/latest/download/install.ps1 -OutFile install.ps1; .\install.ps1 -Doctor"
Write-Host ""
