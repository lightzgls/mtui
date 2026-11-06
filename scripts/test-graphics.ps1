<#
.SYNOPSIS
Probe the real Windows console helper without opening an account or audio device.
#>
[CmdletBinding()]
param(
    [string]$Exe = (Join-Path (Split-Path -Parent $PSScriptRoot) 'target/release/mtui.exe'),
    [switch]$ExpectSixel
)
$ErrorActionPreference = 'Stop'
$taskExe = (Resolve-Path -LiteralPath $Exe).Path
$taskOldTerminal = [Environment]::GetEnvironmentVariable('MTUI_TERMINAL', 'Process')
$taskOldGraphics = [Environment]::GetEnvironmentVariable('MTUI_GRAPHICS', 'Process')
try {
    [Environment]::SetEnvironmentVariable('MTUI_GRAPHICS', $null, 'Process')
    foreach ($taskMode in @('automatic', 'native')) {
        $taskValue = if ($taskMode -eq 'native') { 'native' } else { $null }
        [Environment]::SetEnvironmentVariable('MTUI_TERMINAL', $taskValue, 'Process')
        $taskJson = (& $taskExe --diagnose-graphics | Out-String).Trim()
        if ($LASTEXITCODE -ne 0) { throw "Graphics probe failed for $taskMode : $taskJson" }
        $taskResult = $taskJson | ConvertFrom-Json
        if ($taskResult.cell_width -le 0 -or $taskResult.cell_height -le 0) {
            throw "Invalid cell size: $taskJson"
        }
        if ($ExpectSixel -and $taskMode -eq 'automatic' -and -not $taskResult.sixel) {
            throw "Windows Terminal did not report SIXEL: $taskJson"
        }
        Write-Output "$taskMode : $taskJson"
    }
} finally {
    [Environment]::SetEnvironmentVariable('MTUI_TERMINAL', $taskOldTerminal, 'Process')
    [Environment]::SetEnvironmentVariable('MTUI_GRAPHICS', $taskOldGraphics, 'Process')
}
