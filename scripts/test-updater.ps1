<#
.SYNOPSIS
Verify portable updating and restart against isolated dummy applications.
.DESCRIPTION
Build MTUI first. This uses its private updater helper without opening the UI,
running an installer, touching account data, or replacing the real player.
#>
[CmdletBinding()]
param([string]$Executable = (Join-Path $PSScriptRoot '../target/release/mtui.exe'))

$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This check requires Windows.' }
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskExecutable = (Resolve-Path -LiteralPath $Executable).Path
$taskRun = Join-Path $taskRoot ('target/updater-smoke/' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $taskRun -Force | Out-Null
$taskOldSource = Join-Path $taskRun 'old.rs'
$taskNewSource = Join-Path $taskRun 'new.rs'
$taskTarget = Join-Path $taskRun 'portable.exe'
$taskPayload = Join-Path $taskRun 'new.exe'
$taskHelper = Join-Path $taskRun 'update-helper.exe'
$taskManifest = Join-Path $taskRun 'pending-update.json'
$taskMarker = Join-Path $taskRun 'restarted.txt'
$taskParent = $null
$taskProcess = $null

@'
#![windows_subsystem = "windows"]
fn main() { std::thread::sleep(std::time::Duration::from_secs(3)); }
'@ | Set-Content -LiteralPath $taskOldSource -Encoding utf8
@'
#![windows_subsystem = "windows"]
fn main() {
    let marker = std::env::current_exe().unwrap().parent().unwrap().join("restarted.txt");
    std::fs::write(marker, "updated and restarted").unwrap();
}
'@ | Set-Content -LiteralPath $taskNewSource -Encoding utf8
& rustc --edition 2024 --crate-name old_fixture $taskOldSource -o $taskTarget
if ($LASTEXITCODE -ne 0) { throw 'Could not compile the old fixture.' }
& rustc --edition 2024 --crate-name new_fixture $taskNewSource -o $taskPayload
if ($LASTEXITCODE -ne 0) { throw 'Could not compile the updated fixture.' }
Copy-Item -LiteralPath $taskExecutable -Destination $taskHelper

try {
    $taskParent = Start-Process -FilePath $taskTarget -PassThru -WindowStyle Hidden
    $taskDigest = (Get-FileHash -LiteralPath $taskPayload -Algorithm SHA256).Hash.ToLowerInvariant()
    @{
        executable = $taskTarget; payload = $taskPayload; digest = "sha256:$taskDigest"
        size = (Get-Item -LiteralPath $taskPayload).Length
        kind = 'Portable'; parent = $taskParent.Id
    } | ConvertTo-Json | Set-Content -LiteralPath $taskManifest -Encoding utf8NoBOM
    $taskArguments = @('--mtui-update-helper', ('"' + $taskManifest + '"'))
    $taskProcess = Start-Process -FilePath $taskHelper -ArgumentList $taskArguments -PassThru -WindowStyle Hidden
    if (-not $taskProcess.WaitForExit(30000)) { throw 'The updater did not exit within 30 seconds.' }
    if ($taskProcess.ExitCode -ne 0) { throw "The updater failed with exit code $($taskProcess.ExitCode)." }
    $taskDeadline = [DateTime]::UtcNow.AddSeconds(5)
    while (-not (Test-Path -LiteralPath $taskMarker) -and [DateTime]::UtcNow -lt $taskDeadline) {
        Start-Sleep -Milliseconds 50
    }
    if (-not (Test-Path -LiteralPath $taskMarker)) { throw 'The updated executable was not restarted.' }
    if ((Get-FileHash -LiteralPath $taskTarget -Algorithm SHA256).Hash.ToLowerInvariant() -ne $taskDigest) {
        throw 'The portable executable does not match the verified package.'
    }
    if (Test-Path -LiteralPath $taskManifest) { throw 'The completed manifest was not removed.' }
    Write-Host 'Portable update, parent exit, replacement, checksum, and automatic restart passed.'
    Write-Host "Evidence: $taskRun"
} finally {
    foreach ($taskOwnedProcess in @($taskParent, $taskProcess)) {
        if ($null -ne $taskOwnedProcess -and -not $taskOwnedProcess.HasExited) {
            Stop-Process -Id $taskOwnedProcess.Id -ErrorAction SilentlyContinue
        }
    }
}
