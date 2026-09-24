#Requires -Version 5.1
<#
.SYNOPSIS
Install checkweave into WSL (Windows Subsystem for Linux).

.DESCRIPTION
Checkweave runs on Windows through WSL2. This script checks that WSL is
available, verifies install.sh against the release SHA256SUMS, and runs it
inside the selected distribution. install.sh then verifies and installs the
Linux archive into ~/.local/bin in that distribution.

Review this script before running it. Do not pipe a remote script into
Invoke-Expression. The script does not elevate and does not edit agent
configuration, shell startup files, or workspace data. See docs/usage.md.

.EXAMPLE
.\install.ps1 -Version 0.1.0

.EXAMPLE
.\install.ps1 -Archive .\checkweave-0.1.0-x86_64-unknown-linux-gnu.tar.gz -Checksum <sha256>
#>
[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$Archive = "",
    [string]$Checksum = "",
    [string]$Distro = "",
    [string]$BinDir = ""
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
} catch {
    # Newer PowerShell negotiates TLS 1.2 by default.
}

if (($Version -and $Archive) -or (-not $Version -and -not $Archive)) {
    throw "Pass exactly one of -Version or -Archive."
}
if ($Archive -and -not $Checksum) {
    throw "-Checksum is required for -Archive."
}

function Normalize-Hash([string]$Value) {
    $text = ($Value -replace '\s', '').ToLowerInvariant()
    if ($text -notmatch '^[0-9a-f]{64}$') {
        throw "Checksum must be 64 hex digits."
    }
    return $text
}

function Assert-Sha256([string]$Path, [string]$Expected) {
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
    if ($actual -ne (Normalize-Hash $Expected)) {
        throw "Checksum mismatch for $Path"
    }
}

function Get-ListedHash([string]$SumsPath, [string]$Name) {
    foreach ($line in Get-Content -LiteralPath $SumsPath) {
        if ($line.Trim() -match '^([0-9a-fA-F]{64})\s+\*?(\S+)\s*$') {
            if ($Matches[2] -eq $Name) {
                return $Matches[1].ToLowerInvariant()
            }
        }
    }
    throw "SHA256SUMS has no entry for $Name"
}

function Save-Url([string]$Url, [string]$Dest) {
    $curl = Get-Command curl.exe -ErrorAction SilentlyContinue
    if ($curl) {
        & curl.exe -fsSL --retry 3 --output $Dest $Url
        if ($LASTEXITCODE -ne 0) {
            throw "Download failed: $Url"
        }
        return
    }
    Invoke-WebRequest -Uri $Url -OutFile $Dest -UseBasicParsing -Headers @{
        "User-Agent" = "checkweave-install"
    }
}

function Invoke-Wsl([string[]]$Arguments) {
    $prefix = @()
    if ($Distro) { $prefix = @("-d", $Distro) }
    $output = & wsl.exe @prefix -e @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "WSL command failed ($LASTEXITCODE): $($Arguments -join ' ')"
    }
    return $output
}

function ConvertTo-WslPath([string]$Path) {
    $full = (Resolve-Path -LiteralPath $Path).ProviderPath
    return (Invoke-Wsl @("wslpath", "-a", "-u", $full) | Select-Object -Last 1).Trim()
}

if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
    throw "WSL is not installed. Run 'wsl --install' in an elevated PowerShell, restart, then rerun this script."
}
try {
    $kernel = (Invoke-Wsl @("uname", "-sr") | Select-Object -Last 1).Trim()
} catch {
    throw "WSL has no working Linux distribution. Run 'wsl --install -d Ubuntu', finish its first-run setup, then rerun this script."
}
if ($kernel -notmatch '^Linux ') {
    throw "Unexpected WSL kernel: $kernel"
}
if ($kernel -notmatch 'WSL2|microsoft-standard') {
    Write-Warning "This distribution does not look like WSL2 ($kernel). Checkweave is tested on WSL2; convert with 'wsl --set-version <distro> 2'."
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("checkweave-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $work | Out-Null

try {
    $script = Join-Path $work "install.sh"
    $shArgs = @()
    $shEnv = @()
    if ($BinDir) { $shArgs += @("--bin-dir", $BinDir) }

    if ($Version) {
        if ($Version.StartsWith("v")) { $Version = $Version.Substring(1) }
        if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+[0-9A-Za-z._+-]*$') {
            throw "Invalid version."
        }
        $base = "https://github.com/ruizmr/checkweave/releases/download"
        if ($env:CHECKWEAVE_RELEASE_BASE) {
            $base = $env:CHECKWEAVE_RELEASE_BASE.TrimEnd('/')
            $shEnv += "CHECKWEAVE_RELEASE_BASE=$base"
        }
        $sums = Join-Path $work "SHA256SUMS"
        Save-Url "$base/v$Version/SHA256SUMS" $sums
        Save-Url "$base/v$Version/install.sh" $script
        Assert-Sha256 $script (Get-ListedHash $sums "install.sh")
        $shArgs += @("--version", $Version)
    } else {
        if (-not (Test-Path -LiteralPath $Archive -PathType Leaf)) {
            throw "Archive not found: $Archive"
        }
        $local = Join-Path $PSScriptRoot "install.sh"
        if (-not (Test-Path -LiteralPath $local -PathType Leaf)) {
            throw "install.sh must sit next to install.ps1 for -Archive installs."
        }
        Copy-Item -LiteralPath $local -Destination $script
        $shArgs += @("--archive", (ConvertTo-WslPath $Archive), "--checksum", (Normalize-Hash $Checksum))
    }

    $bytes = [System.IO.File]::ReadAllBytes($script)
    if ([Array]::IndexOf($bytes, [byte]13) -ge 0) {
        throw "install.sh has Windows line endings; download it from the release or check out with LF endings."
    }

    $wslScript = ConvertTo-WslPath $script
    $command = @("env") + $shEnv + @("sh", $wslScript) + $shArgs
    Invoke-Wsl $command | ForEach-Object { Write-Output $_ }
} finally {
    if ($work -like "*checkweave-install-*" -and (Test-Path -LiteralPath $work)) {
        [System.IO.Directory]::Delete($work, $true)
    }
}

Write-Output ""
Write-Output "Checkweave is installed inside WSL. Next steps:"
Write-Output "  1. Keep projects in the WSL filesystem (for example ~/src), not under /mnt/c."
Write-Output "  2. Open the project from WSL: run 'wsl', cd into the project, then 'cursor .'."
Write-Output "  3. In that WSL shell run: checkweave init"
