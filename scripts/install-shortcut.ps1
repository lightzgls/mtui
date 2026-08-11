<#
.SYNOPSIS
    Puts MTUI in the Start menu, so it can be pinned to the taskbar.

.DESCRIPTION
    Windows will not let a program pin itself. The "Pin to taskbar" verb was
    taken away from scripts and installers in Windows 10 1809 and has not come
    back, so pinning is something only a person clicking can do -- and what they
    need to click is a shortcut. This makes the shortcut.

    Afterwards: press Start, type MTUI, right-click the result, and choose
    More > Pin to taskbar.

    The shortcut points at the binary, which keeps its icon and its name
    wherever Windows shows it. Nothing is copied and nothing is registered; the
    shortcut is a single file, and -Uninstall deletes it again.

.PARAMETER Exe
    The mtui.exe to point at. Found automatically if not given: a cargo-installed
    one first, then this repository's release build.

.PARAMETER Desktop
    Also put a copy on the desktop.

.PARAMETER WindowsTerminal
    Add an MTUI profile to Windows Terminal and start MTUI through it, which is
    what puts the icon on the tab. See the notes further down.

.PARAMETER Uninstall
    Remove the shortcuts, and the Windows Terminal profile if one was added.

.EXAMPLE
    powershell -File scripts/install-shortcut.ps1

.EXAMPLE
    powershell -File scripts/install-shortcut.ps1 -Desktop -WindowsTerminal
#>
[CmdletBinding()]
param(
    [string]$Exe,
    [switch]$Desktop,
    [switch]$WindowsTerminal,
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'

# The user's own Start menu, not the machine-wide one: this needs no
# administrator, and MTUI is a program for the person who installed it.
#
# Not $desktop for the second one: PowerShell does not tell that name apart from
# the -Desktop switch, and assigning a path to it fails as a bad conversion to a
# boolean.
$startMenuLink = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\MTUI.lnk'
$desktopLink = Join-Path ([Environment]::GetFolderPath('Desktop')) 'MTUI.lnk'

# Windows Terminal reads profile fragments from here, one directory per program
# that adds them. Per-user, so this needs no administrator either.
$fragmentDir = Join-Path $env:LOCALAPPDATA 'Microsoft\Windows Terminal\Fragments\MTUI'

if ($Uninstall) {
    foreach ($path in @($startMenuLink, $desktopLink)) {
        if (Test-Path -LiteralPath $path) {
            Remove-Item -LiteralPath $path
            Write-Host "removed $path"
        }
    }
    if (Test-Path -LiteralPath $fragmentDir) {
        Remove-Item -LiteralPath $fragmentDir -Recurse -Force
        Write-Host "removed $fragmentDir"
    }
    Write-Host ''
    Write-Host 'If MTUI was pinned to the taskbar, unpin it there as well -- a pinned'
    Write-Host 'shortcut is a copy Windows keeps for itself, and deleting this one'
    Write-Host 'leaves it behind.'
    return
}

$repo = Split-Path -Parent $PSScriptRoot

if (-not $Exe) {
    $candidates = @(
        (Join-Path $env:USERPROFILE '.cargo\bin\mtui.exe'),
        (Join-Path $repo 'target\release\mtui.exe')
    )
    $Exe = $candidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
    if (-not $Exe) {
        throw ('mtui.exe not found. Build it with "cargo build --release", or pass ' +
               '-Exe with the path to it. Looked in:' + "`n  " + ($candidates -join "`n  "))
    }
}

$Exe = (Resolve-Path -LiteralPath $Exe).Path
$target = $Exe
$arguments = ''

if ($WindowsTerminal) {
    $wt = (Get-Command wt.exe -ErrorAction SilentlyContinue).Source
    if (-not $wt) { throw 'Windows Terminal (wt.exe) is not installed, so there is no profile to add it to.' }

    $icon = Join-Path $repo 'assets\mtui.ico'
    if (-not (Test-Path -LiteralPath $icon)) {
        throw "assets\mtui.ico is missing. Rebuild it with scripts/make-icon.ps1."
    }

    if (-not (Test-Path -LiteralPath $fragmentDir)) {
        New-Item -ItemType Directory -Path $fragmentDir | Out-Null
    }
    # Copied rather than referenced where it lies: the profile outlives this
    # checkout, and a profile pointing at a deleted icon draws nothing.
    Copy-Item -LiteralPath $icon -Destination (Join-Path $fragmentDir 'mtui.ico') -Force

    # No guid: Windows Terminal derives a stable one from the fragment's source
    # and the profile name, which is one less thing to collide with a profile
    # somebody already has.
    $fragment = [ordered]@{
        profiles = @(
            [ordered]@{
                name              = 'MTUI'
                commandline       = $Exe
                icon              = (Join-Path $fragmentDir 'mtui.ico')
                startingDirectory = (Split-Path -Parent $Exe)
            }
        )
    }
    $json = Join-Path $fragmentDir 'mtui.json'
    $fragment | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $json -Encoding utf8
    Write-Host "wrote $json"

    # Through the profile rather than straight at the binary, because that is
    # what makes Windows Terminal use the profile -- and its icon -- at all. A
    # console program started any other way is handed to whichever profile is
    # the default, and wears that profile's icon on the tab.
    $target = $wt
    $arguments = '-p MTUI'
}

$targets = @($startMenuLink)
if ($Desktop) { $targets += $desktopLink }

$shell = New-Object -ComObject WScript.Shell
foreach ($path in $targets) {
    $link = $shell.CreateShortcut($path)
    $link.TargetPath = $target
    $link.Arguments = $arguments
    # Nothing MTUI does depends on this -- its config lives in %APPDATA% and the
    # yt-dlp it fetches in %LOCALAPPDATA% -- but a shortcut with no working
    # directory of its own inherits one, and then the program holds a directory
    # open for as long as it runs.
    $link.WorkingDirectory = Split-Path -Parent $Exe
    # Icon 0 of the binary is the one build.rs put there. Named explicitly
    # because with -WindowsTerminal the target is wt.exe, whose icon this is not.
    $link.IconLocation = "$Exe,0"
    $link.Description = 'A terminal music player for YouTube Music'
    $link.Save()
    Write-Host "wrote $path"
}

Write-Host ''
Write-Host 'To pin it: press Start, type MTUI, right-click the result,'
Write-Host 'then More > Pin to taskbar.'
