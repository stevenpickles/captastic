$ErrorActionPreference = 'Stop'

$toolsDirectory = Split-Path -Parent $MyInvocation.MyCommand.Definition
$applicationDirectory = Join-Path $toolsDirectory 'captastic'
$executable = Join-Path $applicationDirectory 'captastic.exe'
$commandLine = Join-Path $applicationDirectory 'captastic-cli.exe'

foreach ($binary in @($executable, $commandLine)) {
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw "The Captastic Chocolatey package is incomplete: $binary is missing."
    }
    Unblock-File -LiteralPath $binary
}

# A user moving from the portable installer should not be left with an old daemon holding
# Captastic's session control event, or a login entry and shortcut that start the old copy.
#
# The portable copy's own uninstall.ps1 is not run: this hook runs elevated, and that script sits
# in a folder the user's unelevated programs can write, so running it would hand any of them
# administrator rights. Its steps are done here instead, and its folder, which holds no settings
# (those are in ~/.captastic), is left for the user to delete.
$legacyDirectory = Join-Path $env:LOCALAPPDATA 'Programs\Captastic'
$legacyExecutable = Join-Path $legacyDirectory 'captastic.exe'
if ((Test-Path -LiteralPath $legacyExecutable -PathType Leaf) -and
    -not [System.IO.Path]::GetFullPath($legacyExecutable).Equals(
        [System.IO.Path]::GetFullPath($executable),
        [System.StringComparison]::OrdinalIgnoreCase
    )) {
    Write-Host 'Moving from the existing per-user Captastic installation.'
    $controlEventName = 'Local\CaptasticDaemonControl-v1'
    $controlEvent = $null
    if ([System.Threading.EventWaitHandle]::TryOpenExisting($controlEventName, [ref]$controlEvent)) {
        try {
            [void]$controlEvent.Set()
        } finally {
            $controlEvent.Dispose()
        }
        $deadline = [DateTime]::UtcNow.AddSeconds(30)
        do {
            Start-Sleep -Milliseconds 100
            $controlEvent = $null
            $running = [System.Threading.EventWaitHandle]::TryOpenExisting($controlEventName, [ref]$controlEvent)
            if ($running) {
                $controlEvent.Dispose()
            }
        } while ($running -and [DateTime]::UtcNow -lt $deadline)
        if ($running) {
            throw 'The running per-user Captastic did not stop within thirty seconds.'
        }
    }
    # Launch at login that started the old copy starts this one instead, rather than being lost.
    $legacyRunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
    $legacyStartup = (Get-ItemProperty -LiteralPath $legacyRunKey -Name 'Captastic' -ErrorAction SilentlyContinue).Captastic
    if ($legacyStartup -and $legacyStartup.Trim().Trim('"').Equals($legacyExecutable, [System.StringComparison]::OrdinalIgnoreCase)) {
        Set-ItemProperty -LiteralPath $legacyRunKey -Name 'Captastic' -Value ('"{0}"' -f $executable)
        Write-Host 'Launch at login now starts the Chocolatey copy of Captastic.'
    }
    $legacyShortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Captastic.lnk'
    if (Test-Path -LiteralPath $legacyShortcut -PathType Leaf) {
        Remove-Item -LiteralPath $legacyShortcut -Force
    }
    Write-Warning "The older per-user Captastic in $legacyDirectory is stopped and no longer starts. Delete that folder when convenient; your settings in ~/.captastic are not in it."
}

# Suppression files in the generated package prevent Chocolatey's automatic shims so each command
# is exposed explicitly. -UseStart makes captastic a GUI shim: captastic.exe is a windowless
# program, and a console shim would hold a console open for as long as Captastic runs.
Install-BinFile -Name 'captastic' -Path $executable -UseStart
# captastic-cli is the opposite: a console program whose output and exit code are the point, so its
# shim waits for it and passes both through (ADR 0014).
Install-BinFile -Name 'captastic-cli' -Path $commandLine
# 0.2 and earlier shipped a separate launcher with a shim of its own.
Uninstall-BinFile -Name 'captastic-desktop'

# Launch at login, as 0.2 and earlier registered it, names a launcher this package no longer
# contains; repoint it at the one executable so the next login still starts Captastic. Only an
# entry for this package's own directory is touched. captastic.exe repeats this check when it
# starts, for the user whose entry this elevated session could not see.
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$legacyLauncher = Join-Path $applicationDirectory 'captastic-desktop.exe'
$startup = (Get-ItemProperty -LiteralPath $runKey -Name 'Captastic' -ErrorAction SilentlyContinue).Captastic
if ($startup -and $startup.Trim().Trim('"').Equals($legacyLauncher, [System.StringComparison]::OrdinalIgnoreCase)) {
    Set-ItemProperty -LiteralPath $runKey -Name 'Captastic' -Value ('"{0}"' -f $executable)
    Write-Host 'Launch at login now starts captastic.exe.'
}

$shortcutPath = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'Captastic.lnk'
Install-ChocolateyShortcut `
    -ShortcutFilePath $shortcutPath `
    -TargetPath $executable `
    -WorkingDirectory $applicationDirectory `
    -IconLocation $executable `
    -Description 'Start Captastic screenshot capture'

# An upgrade that stopped a running Captastic starts it again; a first install never does. The
# before-modify hook of the version being replaced leaves the marker, so upgrades from 0.2, whose
# hook predates it, do not relaunch.
$relaunchMarker = Join-Path $env:TEMP 'captastic-relaunch-after-upgrade'
if (Test-Path -LiteralPath $relaunchMarker -PathType Leaf) {
    $stoppedAt = [DateTime]::MinValue
    $recent = [DateTime]::TryParse(
        (Get-Content -LiteralPath $relaunchMarker -Raw).Trim(),
        [System.Globalization.CultureInfo]::InvariantCulture,
        [System.Globalization.DateTimeStyles]::RoundtripKind,
        [ref]$stoppedAt
    ) -and ([DateTime]::UtcNow - $stoppedAt.ToUniversalTime()) -lt [TimeSpan]::FromMinutes(10)
    Remove-Item -LiteralPath $relaunchMarker -Force
    # This script is elevated, and a tray application started from it would be too. explorer.exe
    # hands the start to the desktop shell already running in this session, which starts Captastic
    # as the signed-in user at their normal integrity level. Without that shell there is nobody to
    # hand it to, and starting explorer.exe here would only start an elevated one.
    $sessionId = (Get-Process -Id $PID).SessionId
    $shell = Get-Process -Name explorer -ErrorAction SilentlyContinue |
        Where-Object { $_.SessionId -eq $sessionId }
    if ($recent -and $shell) {
        Start-Process -FilePath (Join-Path $env:WINDIR 'explorer.exe') -ArgumentList ('"{0}"' -f $executable)
        Write-Host 'Captastic was running before the upgrade and has been started again.'
    } elseif ($recent) {
        Write-Host 'Captastic was running before the upgrade. Start it again from the Start Menu.'
    }
}


Write-Host 'Captastic is installed. Start it from the Start Menu if it is not already running.'
Write-Host 'captastic-cli asks the running Captastic for captures from a terminal; see captastic-cli --help.'
Write-Host 'Configuration and logs are stored under ~/.captastic.'
