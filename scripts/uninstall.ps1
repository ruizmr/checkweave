#Requires -Version 5.1
<#
.SYNOPSIS
Remove one selected checkweave.exe binary.

.DESCRIPTION
Deletes only the selected file. Does not stop a worker and does not delete
workspace .checkweave caches, agent configuration, source files, or model
downloads. See docs/usage.md for the manual removal procedure. There is no
command yet that edits a managed agent block.
#>
[CmdletBinding()]
param(
    [string]$Bin = "",
    [string]$BinDir = ""
)

$ErrorActionPreference = "Stop"

if (-not $BinDir) {
    if ($env:CHECKWEAVE_BIN_DIR) {
        $BinDir = $env:CHECKWEAVE_BIN_DIR
    } else {
        $BinDir = Join-Path $env:USERPROFILE ".local\bin"
    }
}

if (-not $Bin) {
    $Bin = Join-Path $BinDir "checkweave.exe"
}

if (-not $Bin -or $Bin -eq "/" -or $Bin -eq "\") {
    throw "Refusing empty or root path."
}

if (-not (Test-Path -LiteralPath $Bin)) {
    Write-Output "no binary at $Bin (no other files were modified)"
    exit 0
}

$item = Get-Item -LiteralPath $Bin -Force
if ($item.PSIsContainer) {
    throw "Refusing to remove directory $Bin"
}

Remove-Item -LiteralPath $Bin -Force
Write-Output "removed $Bin (workspace cache and agent configuration were not modified)"
