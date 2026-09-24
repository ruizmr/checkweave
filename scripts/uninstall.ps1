#Requires -Version 5.1
<#
.SYNOPSIS
Remove the checkweave binary that install.ps1 placed inside WSL.

.DESCRIPTION
Deletes only ~/.local/bin/checkweave (or -BinDir) in the selected WSL
distribution. Does not stop a worker and does not delete workspace
.checkweave caches, agent configuration, source files, or model downloads.
See docs/usage.md.
#>
[CmdletBinding()]
param(
    [string]$Distro = "",
    [string]$BinDir = ""
)

$ErrorActionPreference = "Stop"

if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
    Write-Output "WSL is not installed; nothing to remove."
    exit 0
}

$prefix = @()
if ($Distro) { $prefix = @("-d", $Distro) }

$local = Join-Path $PSScriptRoot "uninstall.sh"
if (Test-Path -LiteralPath $local -PathType Leaf) {
    $full = (Resolve-Path -LiteralPath $local).ProviderPath
    $script = (& wsl.exe @prefix -e wslpath -a -u $full | Select-Object -Last 1).Trim()
    if ($LASTEXITCODE -ne 0) { throw "wslpath failed for $full" }
    $shArgs = @("sh", $script)
    if ($BinDir) { $shArgs += @("--bin-dir", $BinDir) }
} else {
    # Same checks as uninstall.sh: remove one regular file, nothing else.
    $body = 'bin="${1:-$HOME/.local/bin}/checkweave"; ' +
        'if [ ! -e "$bin" ] && [ ! -L "$bin" ]; then echo "no binary at $bin (no other files were modified)"; exit 0; fi; ' +
        'if [ ! -f "$bin" ]; then echo "refusing to remove non-file $bin" >&2; exit 1; fi; ' +
        'rm -f -- "$bin" && echo "removed $bin (workspace cache and agent configuration were not modified)"'
    $shArgs = @("sh", "-c", $body, "uninstall")
    if ($BinDir) { $shArgs += $BinDir }
}

& wsl.exe @prefix -e @shArgs
if ($LASTEXITCODE -ne 0) {
    throw "uninstall inside WSL failed ($LASTEXITCODE)"
}
