# Build the admin dashboard SPA into the octos-cli embed dir.
#
# PowerShell equivalent of scripts/build-dashboard.sh.
#
# Usage:
#   .\scripts\build-dashboard.ps1
#   .\scripts\build-dashboard.ps1 -InstallDeps
#   .\scripts\build-dashboard.ps1 --install-deps

[CmdletBinding()]
param(
    [switch]$InstallDeps,
    [Alias("h")]
    [switch]$Help,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$RemainingArgs
)

$ErrorActionPreference = "Stop"

# Accept the bash script's GNU-style flag for parity.
foreach ($arg in $RemainingArgs) {
    switch ($arg) {
        "--install-deps" { $InstallDeps = $true }
        "--help" { $Help = $true }
        "-h" { $Help = $true }
        default { throw "Unknown option: $arg" }
    }
}

$Root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$DashboardDir = Join-Path $Root "dashboard"
$OutDir = Join-Path $Root "crates\octos-cli\static\admin"

function Show-Help {
    Write-Host @"
Usage: .\scripts\build-dashboard.ps1 [-InstallDeps]
       .\scripts\build-dashboard.ps1 [--install-deps]

Builds the dashboard SPA into:
  $OutDir

Options:
  -InstallDeps, --install-deps
      Install Node.js LTS with winget when npm is missing.
"@
}

function Resolve-Npm {
    $cmd = Get-Command npm.cmd -ErrorAction SilentlyContinue
    if ($cmd) {
        return $cmd.Source
    }

    $cmd = Get-Command npm -ErrorAction SilentlyContinue
    if ($cmd) {
        return $cmd.Source
    }

    return $null
}

function Invoke-Checked {
    param(
        [Parameter(Mandatory)]
        [string]$FilePath,
        [Parameter(ValueFromRemainingArguments)]
        [string[]]$CommandArgs
    )

    & $FilePath @CommandArgs
    if ($LASTEXITCODE -ne 0) {
        throw "Command failed with exit code ${LASTEXITCODE}: $FilePath $($CommandArgs -join ' ')"
    }
}

function Install-NodeIfRequested {
    if (-not $InstallDeps) {
        Write-Error @"
npm is required to build dashboard assets.

Install Node.js from:
  https://nodejs.org/en/download

Or re-run:
  .\scripts\build-dashboard.ps1 -InstallDeps
"@
    }

    $winget = Get-Command winget -ErrorAction SilentlyContinue
    if (-not $winget) {
        Write-Error "npm is missing and winget is not available. Install Node.js from https://nodejs.org/en/download"
    }

    Write-Host "==> Installing Node.js LTS with winget"
    Invoke-Checked -FilePath $winget.Source install --id OpenJS.NodeJS.LTS --exact --source winget --accept-package-agreements --accept-source-agreements

    $npm = Resolve-Npm
    if (-not $npm) {
        Write-Error "Node.js was installed, but npm is not available in this PowerShell session. Open a new terminal and re-run this script."
    }

    return $npm
}

if ($Help) {
    Show-Help
    exit 0
}

if (-not (Test-Path (Join-Path $DashboardDir "package.json"))) {
    Write-Error "dashboard/package.json not found at $DashboardDir"
}

$npm = Resolve-Npm
if (-not $npm) {
    $npm = Install-NodeIfRequested
}

Push-Location $DashboardDir
try {
    if (-not (Test-Path "node_modules")) {
        Write-Host "Installing dashboard dependencies..."
        Invoke-Checked -FilePath $npm ci
    }

    Write-Host "Building dashboard into $OutDir"
    $oldOutDir = $env:VITE_OUT_DIR
    $oldBasePath = $env:VITE_BASE_PATH
    try {
        $env:VITE_OUT_DIR = $OutDir
        $env:VITE_BASE_PATH = "/admin/"
        Invoke-Checked -FilePath $npm run build
    }
    finally {
        if ($null -eq $oldOutDir) {
            Remove-Item Env:VITE_OUT_DIR -ErrorAction SilentlyContinue
        } else {
            $env:VITE_OUT_DIR = $oldOutDir
        }

        if ($null -eq $oldBasePath) {
            Remove-Item Env:VITE_BASE_PATH -ErrorAction SilentlyContinue
        } else {
            $env:VITE_BASE_PATH = $oldBasePath
        }
    }
}
finally {
    Pop-Location
}

Write-Host "Dashboard assets synced to $OutDir"
