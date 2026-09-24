#Requires -Version 5.1
<#
.SYNOPSIS
Install the checkweave.exe binary into a user directory.

.DESCRIPTION
Review this script before running it. Do not pipe a remote script into
Invoke-Expression. The script does not elevate, does not write Program Files,
and does not edit agent configuration, shell startup files, or workspace data.
Remote installs verify SHA256SUMS. Local archive and binary installs require
-Checksum. See docs/usage.md.
#>
[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$Archive = "",
    [string]$Binary = "",
    [string]$Checksum = "",
    [string]$BinDir = ""
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
} catch {
    # Windows PowerShell on current GitHub runners already negotiates TLS 1.2.
}

if (-not $BinDir) {
    if ($env:CHECKWEAVE_BIN_DIR) {
        $BinDir = $env:CHECKWEAVE_BIN_DIR
    } else {
        $BinDir = Join-Path $env:USERPROFILE ".local\bin"
    }
}

$modes = 0
if ($Version) { $modes++ }
if ($Archive) { $modes++ }
if ($Binary) { $modes++ }
if ($modes -ne 1) {
    throw "Pass exactly one of -Version, -Archive, or -Binary."
}

function Normalize-Hash([string]$Value) {
    $text = ($Value -replace '\s', '').ToLowerInvariant()
    if ($text -notmatch '^[0-9a-f]{64}$') {
        throw "Checksum must be 64 hex digits."
    }
    return $text
}

function Get-Sha256([string]$Path) {
    return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Assert-Sha256([string]$Path, [string]$Expected) {
    $actual = Get-Sha256 $Path
    $want = Normalize-Hash $Expected
    if ($actual -ne $want) {
        throw "Checksum mismatch for $Path"
    }
}

function Get-ListedHash([string]$SumsPath, [string]$Name) {
    foreach ($line in Get-Content -LiteralPath $SumsPath) {
        $clean = $line.Trim()
        if ($clean -match '^([0-9a-fA-F]{64})\s+\*?(\S+)\s*$') {
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

function Install-Atomic([string]$Source, [string]$DestDir) {
    $item = Get-Item -LiteralPath $Source -Force
    $reparse = [IO.FileAttributes]::ReparsePoint
    if ($item.PSIsContainer -or (($item.Attributes -band $reparse) -eq $reparse)) {
        throw "Source is not a regular file: $Source"
    }
    New-Item -ItemType Directory -Force -Path $DestDir | Out-Null
    $dest = Join-Path $DestDir "checkweave.exe"
    if (Test-Path -LiteralPath $dest) {
        $existing = Get-Item -LiteralPath $dest -Force
        if ($existing.PSIsContainer) {
            throw "Refusing to replace directory $dest"
        }
    }
    $tmp = Join-Path $DestDir (".checkweave.install." + [guid]::NewGuid().ToString("N"))
    try {
        Copy-Item -LiteralPath $Source -Destination $tmp -Force
        if ((Test-Path -LiteralPath $dest) -and ((Get-Sha256 $dest) -eq (Get-Sha256 $tmp))) {
            Write-Output "already installed: $dest"
            return
        }
        Move-Item -LiteralPath $tmp -Destination $dest -Force
        Write-Output "installed: $dest"
    } finally {
        if (Test-Path -LiteralPath $tmp) {
            Remove-Item -LiteralPath $tmp -Force
        }
    }
}

function Assert-ZipEntries([string]$Path) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($Path)
    try {
        foreach ($entry in $zip.Entries) {
            $name = $entry.FullName.Replace('\', '/')
            if ($name.StartsWith('/') -or $name -match '(^|/)\.\.(/|$)') {
                throw "Archive contains an unsafe path: $($entry.FullName)"
            }
        }
    } finally {
        $zip.Dispose()
    }
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("checkweave-install-" + [guid]::NewGuid().ToString("N"))
if ($work -notlike "*checkweave-install-*") {
    throw "Unexpected temp directory."
}
New-Item -ItemType Directory -Path $work | Out-Null

try {
    if ($Version) {
        if ($Version.StartsWith("v")) { $Version = $Version.Substring(1) }
        if ($Version -notmatch '^[0-9A-Za-z._+-]+$' -or $Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+') {
            throw "Invalid version."
        }
        $arch = $env:PROCESSOR_ARCHITECTURE
        if ($env:PROCESSOR_ARCHITEW6432) { $arch = $env:PROCESSOR_ARCHITEW6432 }
        if ($arch -ne "AMD64") {
            throw "Windows release target is x86_64 only (found $arch). Use install.sh on Linux or macOS."
        }
        $base = "https://github.com/ruizmr/checkweave/releases/download"
        if ($env:CHECKWEAVE_RELEASE_BASE) {
            $base = $env:CHECKWEAVE_RELEASE_BASE.TrimEnd('/')
        }
        $name = "checkweave-$Version-x86_64-pc-windows-msvc.zip"
        $archivePath = Join-Path $work $name
        $sumsPath = Join-Path $work "SHA256SUMS"
        Save-Url "$base/v$Version/$name" $archivePath
        Save-Url "$base/v$Version/SHA256SUMS" $sumsPath
        Assert-Sha256 $archivePath (Get-ListedHash $sumsPath $name)
        $Archive = $archivePath
    }

    if ($Archive) {
        if (-not (Test-Path -LiteralPath $Archive -PathType Leaf)) {
            throw "Archive not found: $Archive"
        }
        if (-not $Version) {
            if (-not $Checksum) { throw "-Checksum is required for -Archive." }
            Assert-Sha256 $Archive $Checksum
        }
        Assert-ZipEntries $Archive
        Expand-Archive -LiteralPath $Archive -DestinationPath $work -Force
        $found = @(Get-ChildItem -LiteralPath $work -Recurse -File -Filter "checkweave.exe")
        if ($found.Count -ne 1) {
            throw "Archive must contain exactly one checkweave.exe."
        }
        Install-Atomic $found[0].FullName $BinDir
    } else {
        if (-not $Checksum) { throw "-Checksum is required for -Binary." }
        if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
            throw "Binary not found: $Binary"
        }
        Assert-Sha256 $Binary $Checksum
        Install-Atomic $Binary $BinDir
    }
} finally {
    if ($work -like "*checkweave-install-*" -and (Test-Path -LiteralPath $work)) {
        [System.IO.Directory]::Delete($work, $true)
    }
}
