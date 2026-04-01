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

    [Parameter(ParameterSetName = 'Doctor', Mandatory)]
    [switch]$Doctor,

    [Parameter(ParameterSetName = 'Uninstall', Mandatory)]
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"

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
        Write-Host "    Run with -Doctor to diagnose:"
        Write-Host "      .\install.ps1 -Doctor"
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
            Hint "Try reinstalling: .\install.ps1"
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
        Hint "Start: octos serve --port 8080"
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
            404 { Err "responds 404 (admin route not found)"; Hint "Binary may be built without 'api' feature" }
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

$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
switch ($arch) {
    "X64" {
        $Triple = "x86_64-pc-windows-msvc"
        Ok "Windows x64 ($Triple)"
    }
    "Arm64" {
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
    @'
{
  "provider": "anthropic",
  "model": "claude-sonnet-4-20250514",
  "api_key_env": "ANTHROPIC_API_KEY"
}
'@ | Set-Content -Path $configPath -Encoding UTF8
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

# ── Summary ───────────────────────────────────────────────────────────
Section "Installation complete!"
Write-Host ""
Write-Host "    Binary:   $octosBin"
Write-Host "    Data dir: $DataDir"
Write-Host "    Config:   $configPath"
Write-Host ""
Write-Host "  Next steps:"
Write-Host "    1. Set your API key:  `$env:ANTHROPIC_API_KEY = 'sk-...'"
Write-Host "    2. Start chatting:    octos chat"
Write-Host "    3. Open dashboard:    octos serve --port 8080"
Write-Host ""
Write-Host "  Troubleshoot: .\install.ps1 -Doctor"
Write-Host ""
