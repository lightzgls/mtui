<#
.SYNOPSIS
Run isolated MTUI checks and retain logs, inventory, and a JSON summary.
.EXAMPLE
./scripts/test.ps1 -Suite Smoke
.EXAMPLE
./scripts/test.ps1 -Suite Full
.EXAMPLE
./scripts/test.ps1 -Suite Stress -Iterations 10 -Threads 8
#>
[CmdletBinding()]
param(
    [ValidateSet('Smoke', 'Full', 'Stress', 'Audio')][string]$Suite = 'Full',
    [ValidateRange(1, 100)][int]$Iterations = 10,
    [ValidateRange(1, 64)][int]$Threads = 4,
    [switch]$Online,
    [switch]$SkipClippy,
    [string]$AudioFixture
)

$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskRunId = (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
$taskRunDir = Join-Path $taskRoot "target/test-runs/$taskRunId"
$taskCargo = (Get-Command cargo -ErrorAction Stop).Source
$taskUtf8 = [System.Text.UTF8Encoding]::new($false)
$taskPhases = [System.Collections.Generic.List[object]]::new()
$taskPreviousEnv = @{}
$taskEnvironment = @{
    APPDATA = 'appdata'; LOCALAPPDATA = 'localappdata';
    XDG_CONFIG_HOME = 'config'; XDG_CACHE_HOME = 'cache';
    TEMP = 'tmp'; TMP = 'tmp'; TMPDIR = 'tmp'
}
$taskStarted = [DateTimeOffset]::Now
$taskExit = 0
$taskPushed = $false
$taskRunnerError = $null
$taskAudioPath = $null

function Invoke-CargoPhase {
    param([string]$Name, [string[]]$CargoArguments)
    $taskLog = Join-Path $taskRunDir "$Name.log"
    $taskWatch = [Diagnostics.Stopwatch]::StartNew()
    Write-Host "Running $Name ..."
    # Native stderr (compiler progress) is diagnostic output, not a PS error.
    $ErrorActionPreference = 'Continue'
    $taskOutput = @(& $taskCargo @CargoArguments 2>&1 | ForEach-Object { $_.ToString() })
    $taskCode = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    [IO.File]::WriteAllLines($taskLog, [string[]]$taskOutput, $taskUtf8)
    $taskWatch.Stop()
    $taskPassed = 0; $taskFailed = 0; $taskIgnored = 0; $taskFiltered = 0
    foreach ($taskLine in $taskOutput) {
        if ($taskLine -match '^test result: .*? (\d+) passed; (\d+) failed; (\d+) ignored; \d+ measured; (\d+) filtered out;') {
            $taskPassed += [int]$Matches[1]; $taskFailed += [int]$Matches[2]
            $taskIgnored += [int]$Matches[3]; $taskFiltered += [int]$Matches[4]
        }
    }
    if ($taskCode -eq 0 -and $Name -notin @('inventory', 'ignored-inventory', 'clippy') -and ($taskPassed + $taskFailed) -eq 0) {
        $taskCode = 1
        $taskOutput += 'No runnable tests matched this phase.'
        [IO.File]::WriteAllLines($taskLog, [string[]]$taskOutput, $taskUtf8)
    }
    $taskPhases.Add([pscustomobject]@{
        name = $Name; arguments = $CargoArguments; exit_code = $taskCode
        seconds = [Math]::Round($taskWatch.Elapsed.TotalSeconds, 2)
        passed = $taskPassed; failed = $taskFailed; ignored = $taskIgnored
        filtered = $taskFiltered; log = "$Name.log"
    })
    Write-Host "$Name : exit=$taskCode, passed=$taskPassed, failed=$taskFailed, ignored=$taskIgnored"
    if ($taskCode -ne 0) {
        $script:taskExit = 1
        $taskOutput | Select-Object -Last 65 | ForEach-Object { Write-Host $_ }
    }
}

try {
    New-Item -ItemType Directory -Path $taskRunDir -Force | Out-Null
    foreach ($taskKey in $taskEnvironment.Keys) {
        $taskPreviousEnv[$taskKey] = [Environment]::GetEnvironmentVariable($taskKey, 'Process')
        $taskPath = Join-Path $taskRunDir $taskEnvironment[$taskKey]
        New-Item -ItemType Directory -Path $taskPath -Force | Out-Null
        [Environment]::SetEnvironmentVariable($taskKey, $taskPath, 'Process')
    }
    if ($Suite -eq 'Audio') {
        if ([string]::IsNullOrWhiteSpace($AudioFixture) -or -not (Test-Path -LiteralPath $AudioFixture -PathType Leaf)) {
            throw 'Audio requires -AudioFixture pointing to a local AAC/MP4 file.'
        }
        $taskAudioPath = (Resolve-Path -LiteralPath $AudioFixture).ProviderPath
        $taskPreviousEnv['MTUI_AUDIO_FIXTURE'] = [Environment]::GetEnvironmentVariable('MTUI_AUDIO_FIXTURE', 'Process')
        [Environment]::SetEnvironmentVariable('MTUI_AUDIO_FIXTURE', $taskAudioPath, 'Process')
    }
    Push-Location $taskRoot
    $taskPushed = $true
    $taskBase = @('test', '--workspace', '--all-targets', '--locked')
    if (-not $Online) { $taskBase += '--offline' }
    Invoke-CargoPhase 'inventory' ($taskBase + @('--', '--list'))
    Invoke-CargoPhase 'ignored-inventory' ($taskBase + @('--', '--list', '--ignored'))

    if ($Suite -eq 'Smoke') {
        foreach ($taskFilter in @('app::scenarios::', 'player::tests::', 'session::renewal::', 'ui::tests::')) {
            $taskName = 'smoke-' + ($taskFilter -replace '::', '-').TrimEnd('-')
            Invoke-CargoPhase $taskName ($taskBase + @($taskFilter, '--', "--test-threads=$Threads"))
        }
    } elseif ($Suite -eq 'Audio') {
        # Only these local-fixture probes. Never run all ignored tests.
        $taskAudioTests = @(
            'player::decoder::packet_tests::real_aac_resume_matches_the_original_audio_at_eight_seconds',
            'player::tests::initial_403_recovers_once_and_stop_cancels_a_late_replacement'
        )
        for ($taskIndex = 0; $taskIndex -lt $taskAudioTests.Count; $taskIndex++) {
            Invoke-CargoPhase "audio-$($taskIndex + 1)" ($taskBase + @($taskAudioTests[$taskIndex], '--', '--exact', '--ignored', '--test-threads=1'))
        }
    } else {
        $taskCount = if ($Suite -eq 'Stress') { $Iterations } else { 1 }
        for ($taskIteration = 1; $taskIteration -le $taskCount; $taskIteration++) {
            Invoke-CargoPhase "tests-$taskIteration" ($taskBase + @('--', "--test-threads=$Threads"))
        }
        if ($Suite -eq 'Full' -and -not $SkipClippy) {
            $taskLint = @('clippy', '--workspace', '--all-targets', '--locked')
            if (-not $Online) { $taskLint += '--offline' }
            Invoke-CargoPhase 'clippy' ($taskLint + @('--', '-D', 'warnings'))
        }
    }
} catch {
    $taskExit = 1
    $taskRunnerError = $_.ToString()
    Write-Host "Test runner failed: $_"
} finally {
    # Restore the caller's environment even when cargo or a test fails.
    if ($taskPushed) { Pop-Location }
    foreach ($taskKey in $taskPreviousEnv.Keys) {
        [Environment]::SetEnvironmentVariable($taskKey, $taskPreviousEnv[$taskKey], 'Process')
    }
    if (Test-Path -LiteralPath $taskRunDir) {
        $taskCommit = (& git -C $taskRoot rev-parse HEAD 2>$null | Out-String).Trim()
        $taskChanges = @(& git -C $taskRoot status --porcelain 2>$null)
        $taskIgnoredExecuted = @($taskPhases.ToArray() | Where-Object {
            $_.name -like 'audio-*' -and ($_.passed + $_.failed) -gt 0
        }).Count -gt 0
        $taskReport = [ordered]@{
            suite = $Suite; started_at = $taskStarted.ToString('o')
            finished_at = [DateTimeOffset]::Now.ToString('o'); exit_code = $taskExit
            commit = $taskCommit; working_tree_dirty = ($taskChanges.Count -gt 0)
            platform = [Environment]::OSVersion.ToString(); threads = $Threads
            dependency_downloads_allowed = [bool]$Online
            isolated_profile = $true; ignored_tests_executed = $taskIgnoredExecuted
            audio_fixture = $taskAudioPath
            runner_error = $taskRunnerError
            phases = @($taskPhases.ToArray())
        }
        [IO.File]::WriteAllText((Join-Path $taskRunDir 'summary.json'), ($taskReport | ConvertTo-Json -Depth 8), $taskUtf8)
        Write-Host "Results: $taskRunDir"
    }
}
exit $taskExit
