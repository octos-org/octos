#!/usr/bin/env pwsh
# Regression test for the #2514 bundle checksum verification in
# scripts/install.ps1: the .sha256 sidecar that bundle-release.sh publishes
# next to every bundle must be enforced before the bundle is extracted, and a
# missing or unparseable sidecar (pre-rc.12 releases, 200-with-HTML mirrors)
# must not abort. Extracts the actual Test-BundleChecksum function from
# install.ps1 so the test tracks the real implementation, and pins the call
# site so the download flow cannot silently drop verification. Cross-platform
# (Get-FileHash); also runs on Windows PowerShell 5.1.

$ErrorActionPreference = "Stop"

$installPs1 = Join-Path $PSScriptRoot ".." "install.ps1"
$raw = Get-Content $installPs1 -Raw
$m = [regex]::Match($raw, '(?s)^function Test-BundleChecksum\b.*?^}', [System.Text.RegularExpressions.RegexOptions]::Multiline)
if (-not $m.Success) {
    throw "Test-BundleChecksum not found in $installPs1 — install.ps1 lost its checksum verification"
}
# The download flow must fetch the sidecar and must verify the bundle
# BEFORE extraction; without these pins a refactor could silently drop the
# fetch or verify after Expand-Archive, both green.
if ($raw -notmatch '"\$DownloadUrl\.sha256"') {
    throw "install.ps1 no longer fetches the .sha256 sidecar alongside the bundle"
}
$callIdx = $raw.IndexOf('Test-BundleChecksum $zipPath')
$extractIdx = $raw.IndexOf('Expand-Archive -Path $zipPath -DestinationPath $extractDir')
if ($callIdx -lt 0 -or $extractIdx -lt 0 -or $callIdx -gt $extractIdx) {
    throw "install.ps1 must verify the bundle checksum before extraction"
}
Invoke-Expression $m.Value

# Test-local stubs for the installer's output helpers; Err aborts the real
# install, so here it throws so the test can observe the refusal.
function Ok([string]$Message) { Write-Host "    OK: $Message" }
function Warn([string]$Message) { Write-Host "    WARN: $Message" }
function Err([string]$Message) { throw "Err: $Message" }

$dir = Join-Path ([System.IO.Path]::GetTempPath()) ("octos-checksum-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $dir | Out-Null
try {
    $zip = Join-Path $dir "octos-bundle-test.zip"
    Set-Content -Path $zip -Value "payload" -Encoding ascii
    $sidecar = "$zip.sha256"
    # Sidecar line exactly as bundle-release.sh writes it: "<hex>  <filename>".
    $hash = (Get-FileHash -Path $zip -Algorithm SHA256).Hash.ToLower()
    Set-Content -Path $sidecar -Value "$hash  octos-bundle-test.zip" -Encoding ascii

    # Happy path: matching sidecar verifies.
    Test-BundleChecksum $zip $sidecar

    # Uppercase hash: what Get-FileHash itself prints. Must verify too.
    Set-Content -Path $sidecar -Value ("$hash".ToUpper() + "  octos-bundle-test.zip") -Encoding ascii
    Test-BundleChecksum $zip $sidecar

    # Mismatched sidecar must abort the install.
    Set-Content -Path $sidecar -Value (("0" * 64) + "  octos-bundle-test.zip") -Encoding ascii
    $refused = $false
    try { Test-BundleChecksum $zip $sidecar } catch { $refused = $true }
    if (-not $refused) {
        throw "mismatched sidecar did not abort the install"
    }

    # Missing sidecar must warn and continue, never abort.
    Remove-Item $sidecar
    Test-BundleChecksum $zip "$sidecar.absent"

    # Unparseable sidecars (0 bytes, or a 200-with-HTML mirror answer) must
    # warn and continue, never abort with a raw error or a bogus mismatch.
    foreach ($garbage in @("", "<html><body>404: not found</body></html>")) {
        if ($garbage -eq "") {
            [System.IO.File]::WriteAllText($sidecar, "")
        } else {
            Set-Content -Path $sidecar -Value $garbage -Encoding ascii
        }
        Test-BundleChecksum $zip $sidecar
    }

    Write-Host "install checksum tests passed"
} finally {
    Remove-Item $dir -Recurse -Force
}
