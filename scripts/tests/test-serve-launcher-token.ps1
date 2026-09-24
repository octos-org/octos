#!/usr/bin/env pwsh
# Regression test for the #2388 serve-launcher token handoff: the
# serve-launcher.cmd wrapper must read OCTOS_AUTH_TOKEN from the
# ACL-restricted sibling serve-token file, never embed it inline, and
# refuse to start when the token file is missing. Runs the actual
# template from scripts/install.ps1 under a real cmd.exe, so it is
# Windows-only (cmd.exe + icacls); other platforms skip.

$ErrorActionPreference = "Stop"

if ($PSVersionTable.PSVersion.Major -lt 7) {
    throw "run under pwsh 7 (shell: pwsh); Windows PowerShell 5.1 lacks `$IsWindows"
}
if (-not $IsWindows) {
    Write-Host "skip: windows-only (cmd.exe + icacls)"
    exit 0
}

$installPs1 = Join-Path $PSScriptRoot ".." "install.ps1"
$raw = Get-Content $installPs1 -Raw
$m = [regex]::Match($raw, '(?s)\$wrapperContent = @"\r?\n(.*?)\r?\n"@\r?\n')
if (-not $m.Success) {
    throw "wrapper template not found in $installPs1"
}
$template = $m.Groups[1].Value

$dir = Join-Path ([System.IO.Path]::GetTempPath()) ("octos-launcher-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $dir | Out-Null
try {
    # The restricted token file, exactly as install.ps1 writes it.
    $tokenPath = Join-Path $dir "serve-token"
    [System.IO.File]::WriteAllText($tokenPath, "check-token-123", [System.Text.UTF8Encoding]::new($false))
    icacls $tokenPath /inheritance:r /grant:r "${env:USERNAME}:F" "*S-1-5-18:F" "*S-1-5-32-544:F" | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "icacls failed on the serve-token file"
    }
    $acl = icacls $tokenPath | Out-String
    Write-Host $acl
    if ($acl -match "Everyone|BUILTIN\\Users") {
        throw "serve-token file is readable by other local users"
    }

    # A stand-in serve binary: dump the env the launcher handed over.
    $fakeBin = Join-Path $dir "octos.cmd"
    Set-Content -Path $fakeBin -Value '@echo TOKEN=%OCTOS_AUTH_TOKEN% DATADIR=%OCTOS_DATA_DIR%' -Encoding ascii

    $DataDir = $dir
    $octosBin = $fakeBin
    $Port = 8080
    $serveLog = Join-Path $dir "serve.log"

    # Expand the real install.ps1 template (same variables, same bytes).
    $wrapperContent = $ExecutionContext.InvokeCommand.ExpandString($template)
    $wrapperPath = Join-Path $dir "serve-launcher.cmd"
    [System.IO.File]::WriteAllText($wrapperPath, $wrapperContent, [System.Text.UTF8Encoding]::new($false))

    if ($wrapperContent -match "check-token-123") {
        throw "launcher template embeds the token inline"
    }

    # Happy path: the token reaches the serve process via the env file.
    cmd.exe /C "`"$wrapperPath`"" | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "launcher exited $LASTEXITCODE with the token file present"
    }
    $log = Get-Content $serveLog -Raw
    Write-Host $log
    if ($log -notmatch "TOKEN=check-token-123") {
        throw "launcher did not hand OCTOS_AUTH_TOKEN to the serve process"
    }
    if ($log -notmatch [regex]::Escape("DATADIR=$dir")) {
        throw "launcher lost OCTOS_DATA_DIR"
    }

    # Negative arm: no token file -> refuse with a diagnostic, never start
    # with an empty token.
    Remove-Item $tokenPath
    Remove-Item $serveLog -ErrorAction SilentlyContinue
    cmd.exe /C "`"$wrapperPath`"" | Out-Null
    $missing = Get-Content $serveLog -Raw
    Write-Host $missing
    if ($missing -notmatch "serve-token file missing or empty") {
        throw "launcher did not report the missing serve-token file"
    }

    # deploy.ps1's NSSM wrapper must satisfy the same contract (#2496):
    # token read from the restricted file, never inline. NSSM delivers
    # the non-secret env via AppEnvironmentExtra, so simulate that here.
    $deployPs1 = Join-Path $PSScriptRoot ".." "deploy.ps1"
    $deployRaw = Get-Content $deployPs1 -Raw
    $m2 = [regex]::Match($deployRaw, '(?s)Write-Utf8NoBom \$wrapperPath @"\r?\n(.*?)\r?\n"@\r?\n')
    if (-not $m2.Success) {
        throw "serve-launcher template not found in $deployPs1"
    }
    $deployTemplate = $m2.Groups[1].Value
    if ($deployTemplate -match "check-token-123") {
        throw "deploy launcher template embeds the token inline"
    }

    [System.IO.File]::WriteAllText($tokenPath, "check-token-123", [System.Text.UTF8Encoding]::new($false))
    $env:OCTOS_HOME = $dir
    $env:OCTOS_DATA_DIR = $dir
    $octosExe = $fakeBin
    $servePort = 8080
    $deployContent = $ExecutionContext.InvokeCommand.ExpandString($deployTemplate)
    $deployWrapperPath = Join-Path $dir "deploy-serve-launcher.cmd"
    [System.IO.File]::WriteAllText($deployWrapperPath, $deployContent, [System.Text.UTF8Encoding]::new($false))

    $happy = (cmd.exe /C "`"$deployWrapperPath`"" | Out-String)
    Write-Host $happy
    if ($LASTEXITCODE -ne 0) {
        throw "deploy launcher exited $LASTEXITCODE with the token file present"
    }
    if ($happy -notmatch "TOKEN=check-token-123") {
        throw "deploy launcher did not hand OCTOS_AUTH_TOKEN to the serve process"
    }
    if ($happy -notmatch [regex]::Escape("DATADIR=$dir")) {
        throw "deploy launcher lost the service environment"
    }

    Remove-Item $tokenPath
    $refusal = (cmd.exe /C "`"$deployWrapperPath`"" | Out-String)
    Write-Host $refusal
    if ($LASTEXITCODE -ne 1) {
        throw "deploy launcher must exit 1 when the serve-token file is missing"
    }
    if ($refusal -notmatch "serve-token file missing or empty") {
        throw "deploy launcher did not report the missing serve-token file"
    }
    Remove-Item Env:OCTOS_HOME -ErrorAction SilentlyContinue
    Remove-Item Env:OCTOS_DATA_DIR -ErrorAction SilentlyContinue

    Write-Host "ok: serve-launcher token handoff"
} finally {
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
}
