# deploy.ps1 - Deploy Octos to a remote Windows server over OpenSSH.
#
# This is the Windows counterpart to the shell deploy flows. It runs from an
# operator machine with PowerShell and OpenSSH, connects to a Windows target via
# ssh/scp, installs the release bundle under C:\octos by default, and registers
# octos serve as an auto-start Windows service through NSSM.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$HostName,

    [string]$User = "",
    [int]$Port = 22,
    [string]$IdentityFile = "",
    [string]$Version = "latest",
    [string]$RemoteRoot = "C:\octos",
    [string]$ServiceName = "OctosServe",
    [int]$ServePort = 8080,
    [string]$AuthToken = "",
    [string]$DownloadBase = "",
    [string]$LocalBundle = "",
    [switch]$InstallDeps,
    [switch]$Restart,
    [switch]$Uninstall,
    [switch]$Purge,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

$GithubRepo = "octos-org/octos"
$BundleName = "octos-bundle-x86_64-pc-windows-msvc.zip"
$NssmVersion = "2.24"

function Section([string]$Message) {
    Write-Host ""
    Write-Host "==> $Message"
}

function Ok([string]$Message) {
    Write-Host "    OK: $Message"
}

function Fail([string]$Message) {
    throw $Message
}

function Assert-Command([string]$Name) {
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        Fail "required command not found: $Name"
    }
}

function ConvertTo-PSLiteral([string]$Value) {
    return "'" + ($Value -replace "'", "''") + "'"
}

function ConvertTo-EncodedPowerShellCommand([string]$ScriptText) {
    $bytes = [System.Text.Encoding]::Unicode.GetBytes($ScriptText)
    return [Convert]::ToBase64String($bytes)
}

function Get-ReleaseBase {
    if ($DownloadBase) {
        return $DownloadBase.TrimEnd("/")
    }
    if ($Version -eq "latest") {
        return "https://github.com/$GithubRepo/releases/latest/download"
    }
    return "https://github.com/$GithubRepo/releases/download/$Version"
}

function Get-Target {
    if ($User) {
        return "${User}@${HostName}"
    }
    return $HostName
}

function Get-SshArgs {
    $cmdArgs = @()
    if ($Port -ne 22) {
        $cmdArgs += @("-p", "$Port")
    }
    if ($IdentityFile) {
        $cmdArgs += @("-i", $IdentityFile)
    }
    return $cmdArgs
}

function Get-ScpArgs {
    $cmdArgs = @()
    if ($Port -ne 22) {
        $cmdArgs += @("-P", "$Port")
    }
    if ($IdentityFile) {
        $cmdArgs += @("-i", $IdentityFile)
    }
    return $cmdArgs
}

function Format-Command([string]$Exe, [string[]]$CommandArgs) {
    $parts = @($Exe)
    foreach ($arg in $CommandArgs) {
        if ($arg -match '^[A-Za-z0-9_./:@=+,-]+$') {
            $parts += $arg
        } else {
            $parts += '"' + ($arg -replace '"', '\"') + '"'
        }
    }
    return ($parts -join " ")
}

function Invoke-Native([string]$Exe, [string[]]$CommandArgs) {
    if ($DryRun) {
        Write-Output "    [dry-run] $(Format-Command $Exe $CommandArgs)"
        return
    }

    & $Exe @CommandArgs
    if ($LASTEXITCODE -ne 0) {
        Fail "$Exe exited with code $LASTEXITCODE"
    }
}

function ConvertTo-ScpRemotePath([string]$Path) {
    return ($Path -replace "\\", "/")
}

function Copy-ToRemote([string]$LocalPath, [string]$RemotePath) {
    $target = Get-Target
    $remote = "$(ConvertTo-ScpRemotePath $RemotePath)"
    $cmdArgs = (Get-ScpArgs) + @($LocalPath, "${target}:$remote")
    Invoke-Native "scp" $cmdArgs
}

function Invoke-RemoteScript([string]$ScriptText) {
    $target = Get-Target
    $encoded = ConvertTo-EncodedPowerShellCommand $ScriptText
    $remoteCommand = "powershell -NoProfile -ExecutionPolicy Bypass -EncodedCommand $encoded"

    if ($DryRun) {
        Write-Output "    [dry-run] remote PowerShell script:"
        foreach ($line in ($ScriptText -split "`r?`n")) {
            if ($line.Trim().Length -gt 0) {
                Write-Output "      $line"
            }
        }
    }

    $cmdArgs = (Get-SshArgs) + @($target, $remoteCommand)
    Invoke-Native "ssh" $cmdArgs
}

function New-RemoteDeployScript([string]$UploadedBundlePath) {
    $releaseBase = Get-ReleaseBase
    $bundleUrl = "$releaseBase/$BundleName"
    $nssmUrl = "https://nssm.cc/release/nssm-$NssmVersion.zip"

    $template = @'
$ErrorActionPreference = "Stop"

$remoteRoot = __REMOTE_ROOT__
$serviceName = __SERVICE_NAME__
$servePort = __SERVE_PORT__
$authToken = __AUTH_TOKEN__
$bundleUrl = __BUNDLE_URL__
$uploadedBundle = __UPLOADED_BUNDLE__
$bundleName = __BUNDLE_NAME__
$nssmUrl = __NSSM_URL__
$installDeps = __INSTALL_DEPS__
$restartOnly = __RESTART_ONLY__

function Section([string]$Message) {
    Write-Host ""
    Write-Host "==> $Message"
}

function Ok([string]$Message) {
    Write-Host "    OK: $Message"
}

function Ensure-Directory([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) {
        New-Item -ItemType Directory -Path $Path -Force | Out-Null
    }
}

function Write-Utf8NoBom([string]$Path, [string]$Content) {
    [System.IO.File]::WriteAllText($Path, $Content, [System.Text.UTF8Encoding]::new($false))
}

function New-AuthToken {
    $bytes = New-Object byte[] 32
    ([System.Security.Cryptography.RandomNumberGenerator]::Create()).GetBytes($bytes)
    return ($bytes | ForEach-Object { $_.ToString("x2") }) -join ""
}

$binDir = Join-Path $remoteRoot "bin"
$dataDir = Join-Path $remoteRoot "data"
$logDir = Join-Path $remoteRoot "logs"
$tmpDir = Join-Path $remoteRoot "tmp"

Ensure-Directory $binDir
Ensure-Directory $dataDir
Ensure-Directory $logDir
Ensure-Directory $tmpDir

if (-not $authToken) {
    $authToken = New-AuthToken
}

$octosExe = Join-Path $binDir "octos.exe"
$nssmExe = Join-Path $binDir "nssm.exe"

if ($installDeps) {
    Section "Installing runtime dependencies"
    if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
        Write-Host "    WARN: winget not found; install Git, Node.js, Python, and FFmpeg manually if this host needs them"
    } else {
        foreach ($pkg in @("Git.Git", "OpenJS.NodeJS.LTS", "Python.Python.3.12", "Gyan.FFmpeg")) {
            winget install --id $pkg --exact --silent --accept-package-agreements --accept-source-agreements
        }
        Ok "runtime dependency install attempted through winget"
    }
}

if (-not $restartOnly) {
    Section "Installing Octos bundle"
    $bundleZip = Join-Path $tmpDir $bundleName
    if ($uploadedBundle) {
        Copy-Item -LiteralPath $uploadedBundle -Destination $bundleZip -Force
    } else {
        Invoke-WebRequest -Uri $bundleUrl -OutFile $bundleZip -UseBasicParsing
    }

    $extractDir = Join-Path $tmpDir "bundle"
    if (Test-Path -LiteralPath $extractDir) {
        Remove-Item -Recurse -Force $extractDir
    }
    New-Item -ItemType Directory -Path $extractDir -Force | Out-Null
    Expand-Archive -Path $bundleZip -DestinationPath $extractDir -Force

    $octosSource = Get-ChildItem -Path $extractDir -Recurse -Filter "octos.exe" | Select-Object -First 1
    if (-not $octosSource) {
        throw "octos.exe not found in $bundleZip"
    }
    Copy-Item $octosSource.FullName -Destination $octosExe -Force
    Ok "installed $octosExe"
}

$configPath = Join-Path $dataDir "config.json"
if (-not (Test-Path -LiteralPath $configPath)) {
    $config = [ordered]@{
        provider = "openai"
        model = "gpt-4.1-mini"
        api_key_env = "OPENAI_API_KEY"
        mode = "local"
        auth_token = $authToken
    }
    Write-Utf8NoBom $configPath ($config | ConvertTo-Json -Depth 8)
    Ok "created $configPath"
} else {
    Ok "preserving existing $configPath"
}

if (-not (Test-Path -LiteralPath $nssmExe)) {
    Section "Installing NSSM service wrapper"
    $nssmZip = Join-Path $tmpDir "nssm.zip"
    $nssmExtract = Join-Path $tmpDir "nssm"
    Invoke-WebRequest -Uri $nssmUrl -OutFile $nssmZip -UseBasicParsing
    if (Test-Path -LiteralPath $nssmExtract) {
        Remove-Item -Recurse -Force $nssmExtract
    }
    Expand-Archive -Path $nssmZip -DestinationPath $nssmExtract -Force
    $nssmSource = Get-ChildItem -Path $nssmExtract -Recurse -Filter "nssm.exe" |
        Where-Object { $_.FullName -match "\\win64\\" } |
        Select-Object -First 1
    if (-not $nssmSource) {
        throw "win64 nssm.exe not found in $nssmZip"
    }
    Copy-Item $nssmSource.FullName -Destination $nssmExe -Force
    Ok "installed $nssmExe"
}

Section "Registering Windows service"
& $nssmExe stop $serviceName 2>$null | Out-Null
& $nssmExe remove $serviceName confirm 2>$null | Out-Null

& $nssmExe install $serviceName $octosExe "serve" "--host" "0.0.0.0" "--port" "$servePort" "--data-dir" $dataDir "--auth-token" $authToken
if ($LASTEXITCODE -ne 0) {
    throw "nssm.exe install failed"
}

& $nssmExe set $serviceName AppDirectory $remoteRoot | Out-Null
& $nssmExe set $serviceName AppStdout (Join-Path $logDir "serve.log") | Out-Null
& $nssmExe set $serviceName AppStderr (Join-Path $logDir "serve.err.log") | Out-Null
& $nssmExe set $serviceName AppRotateFiles 1 | Out-Null
& $nssmExe set $serviceName Start SERVICE_AUTO_START | Out-Null
& $nssmExe set $serviceName AppEnvironmentExtra "OCTOS_HOME=$dataDir" "OCTOS_DATA_DIR=$dataDir" "OCTOS_AUTH_TOKEN=$authToken" | Out-Null

& $nssmExe start $serviceName
if ($LASTEXITCODE -ne 0) {
    throw "nssm.exe start failed"
}

Ok "$serviceName service started"
Write-Host ""
Write-Host "    Remote root: $remoteRoot"
Write-Host "    Binary:      $octosExe"
Write-Host "    Data dir:    $dataDir"
Write-Host "    Logs:        $logDir"
Write-Host "    Dashboard:   http://$env:COMPUTERNAME`:$servePort/admin/"
'@

    $scriptText = $template
    $scriptText = $scriptText.Replace("__REMOTE_ROOT__", (ConvertTo-PSLiteral $RemoteRoot))
    $scriptText = $scriptText.Replace("__SERVICE_NAME__", (ConvertTo-PSLiteral $ServiceName))
    $scriptText = $scriptText.Replace("__SERVE_PORT__", "$ServePort")
    $scriptText = $scriptText.Replace("__AUTH_TOKEN__", (ConvertTo-PSLiteral $AuthToken))
    $scriptText = $scriptText.Replace("__BUNDLE_URL__", (ConvertTo-PSLiteral $bundleUrl))
    $scriptText = $scriptText.Replace("__UPLOADED_BUNDLE__", (ConvertTo-PSLiteral $UploadedBundlePath))
    $scriptText = $scriptText.Replace("__BUNDLE_NAME__", (ConvertTo-PSLiteral $BundleName))
    $scriptText = $scriptText.Replace("__NSSM_URL__", (ConvertTo-PSLiteral $nssmUrl))
    $scriptText = $scriptText.Replace("__INSTALL_DEPS__", ($(if ($InstallDeps) { '$true' } else { '$false' })))
    $scriptText = $scriptText.Replace("__RESTART_ONLY__", ($(if ($Restart) { '$true' } else { '$false' })))
    return $scriptText
}

function New-RemoteUninstallScript {
    $template = @'
$ErrorActionPreference = "Stop"

$remoteRoot = __REMOTE_ROOT__
$serviceName = __SERVICE_NAME__
$purge = __PURGE__

function Ok([string]$Message) {
    Write-Host "    OK: $Message"
}

$nssmExe = Join-Path (Join-Path $remoteRoot "bin") "nssm.exe"
if (Test-Path -LiteralPath $nssmExe) {
    & $nssmExe stop $serviceName 2>$null | Out-Null
    & $nssmExe remove $serviceName confirm 2>$null | Out-Null
    Ok "removed $serviceName through NSSM"
} else {
    sc.exe stop $serviceName 2>$null | Out-Null
    sc.exe delete $serviceName 2>$null | Out-Null
    Ok "requested $serviceName removal through sc.exe"
}

if ($purge) {
    Remove-Item -Recurse -Force $remoteRoot -ErrorAction SilentlyContinue
    Ok "removed $remoteRoot"
}
'@

    $scriptText = $template
    $scriptText = $scriptText.Replace("__REMOTE_ROOT__", (ConvertTo-PSLiteral $RemoteRoot))
    $scriptText = $scriptText.Replace("__SERVICE_NAME__", (ConvertTo-PSLiteral $ServiceName))
    $scriptText = $scriptText.Replace("__PURGE__", ($(if ($Purge) { '$true' } else { '$false' })))
    return $scriptText
}

function Validate-Inputs {
    if ($Port -lt 1 -or $Port -gt 65535) {
        Fail "Port must be between 1 and 65535"
    }
    if ($ServePort -lt 1 -or $ServePort -gt 65535) {
        Fail "ServePort must be between 1 and 65535"
    }
    if ($ServiceName -notmatch '^[A-Za-z0-9_.-]+$') {
        Fail "ServiceName must contain only letters, numbers, dots, underscores, and dashes"
    }
    if ($LocalBundle -and -not (Test-Path -LiteralPath $LocalBundle)) {
        Fail "LocalBundle not found: $LocalBundle"
    }
}

Validate-Inputs

if (-not $DryRun) {
    Assert-Command "ssh"
    if ($LocalBundle) {
        Assert-Command "scp"
    }
}

Section "Windows deploy target"
Ok "target: $(Get-Target)"
Ok "remote root: $RemoteRoot"
Ok "service: $ServiceName"

$uploadedBundlePath = ""
if ($LocalBundle -and -not $Uninstall) {
    Section "Uploading local bundle"
    $incomingDir = Join-Path $RemoteRoot "incoming"
    $createIncoming = "New-Item -ItemType Directory -Path $(ConvertTo-PSLiteral $incomingDir) -Force | Out-Null"
    Invoke-RemoteScript $createIncoming

    $uploadedBundlePath = Join-Path $incomingDir $BundleName
    Copy-ToRemote $LocalBundle $uploadedBundlePath
}

if ($Uninstall) {
    Section "Uninstalling remote Windows service"
    Invoke-RemoteScript (New-RemoteUninstallScript)
} else {
    Section "Deploying remote Windows service"
    Invoke-RemoteScript (New-RemoteDeployScript $uploadedBundlePath)
}

Ok "deploy.ps1 completed"
