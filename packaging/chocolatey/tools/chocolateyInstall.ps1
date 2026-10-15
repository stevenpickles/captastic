$ErrorActionPreference = 'Stop'

$toolsDirectory = Split-Path -Parent $MyInvocation.MyCommand.Definition
$applicationDirectory = Join-Path $toolsDirectory 'captastic'
$executable = Join-Path $applicationDirectory 'captastic.exe'

if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
    throw "The Captastic Chocolatey package is incomplete: $executable is missing."
}
Unblock-File -LiteralPath $executable

# A user moving from the portable installer should not be left with an old daemon
# holding Captastic's session control event. Its uninstaller preserves ~/.captastic.
$legacyDirectory = Join-Path $env:LOCALAPPDATA 'Programs\Captastic'
$legacyCli = Join-Path $legacyDirectory 'captastic.exe'
$legacyUninstaller = Join-Path $legacyDirectory 'uninstall.ps1'
if ((Test-Path -LiteralPath $legacyCli -PathType Leaf) -and
    -not [System.IO.Path]::GetFullPath($legacyCli).Equals(
        [System.IO.Path]::GetFullPath($executable),
        [System.StringComparison]::OrdinalIgnoreCase
    )) {
    if (Test-Path -LiteralPath $legacyUninstaller -PathType Leaf) {
        Write-Host 'Migrating the existing per-user Captastic installation.'
        & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $legacyUninstaller
        if ($LASTEXITCODE -ne 0) {
            throw "The existing Captastic uninstaller failed with exit code $LASTEXITCODE."
        }
    } else {
        Write-Warning "An older Captastic installation remains at $legacyDirectory. Uninstall it before launching this package."
    }
}

# A suppression file in the generated package prevents Chocolatey's automatic shim so the one
# command is exposed explicitly. -UseStart makes it a GUI shim: captastic.exe is a windowless
# program, and a console shim would hold a console open for as long as Captastic runs.
Install-BinFile -Name 'captastic' -Path $executable -UseStart
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
Write-Host 'Configuration and logs are stored under ~/.captastic.'
