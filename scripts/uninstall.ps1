[CmdletBinding()]
param(
    [string]$InstallDirectory = (Join-Path $env:LOCALAPPDATA 'Programs\Captastic'),
    [switch]$RemoveSettings
)

$ErrorActionPreference = 'Stop'

# Stops a running Captastic through its session control event: the event every version's daemon
# holds while it runs and every version's `stop` command signalled. Going to the event rather than
# to an executable is what lets this script stop a 0.2 daemon and a 0.3 one alike; 0.3 ships no
# command line to ask.
function Stop-Captastic {
    param([int]$TimeoutSeconds = 5)

    $name = 'Local\CaptasticDaemonControl-v1'
    $controlEvent = $null
    if (-not [System.Threading.EventWaitHandle]::TryOpenExisting($name, [ref]$controlEvent)) {
        return
    }
    try {
        [void]$controlEvent.Set()
    } finally {
        $controlEvent.Dispose()
    }
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        Start-Sleep -Milliseconds 100
        $controlEvent = $null
        $running = [System.Threading.EventWaitHandle]::TryOpenExisting($name, [ref]$controlEvent)
        if ($running) {
            $controlEvent.Dispose()
        }
    } while ($running -and [DateTime]::UtcNow -lt $deadline)
    if ($running) {
        throw "The running Captastic daemon did not stop within $TimeoutSeconds seconds."
    }
}

$InstallDirectory = [System.IO.Path]::GetFullPath($InstallDirectory)
$installRoot = [System.IO.Path]::GetPathRoot($InstallDirectory)
if ($InstallDirectory.TrimEnd('\') -eq $installRoot.TrimEnd('\')) {
    throw 'The installation directory cannot be a filesystem root.'
}
# The folder is deleted with everything in it, so it must be a Captastic installation: one that
# install.ps1 made holds captastic.exe and this script. A mistyped -InstallDirectory must not
# delete some other folder.
$isInstallation = (Test-Path -LiteralPath (Join-Path $InstallDirectory 'captastic.exe') -PathType Leaf) -or
    (Test-Path -LiteralPath (Join-Path $InstallDirectory 'uninstall.ps1') -PathType Leaf)
if ((Test-Path -LiteralPath $InstallDirectory -PathType Container) -and -not $isInstallation) {
    throw "$InstallDirectory does not hold a Captastic installation, so nothing was removed."
}
if (Test-Path -LiteralPath (Join-Path $InstallDirectory 'captastic.exe') -PathType Leaf) {
    Stop-Captastic
}

# Only a login entry that starts this installation: one that starts another copy, the Chocolatey
# package say, is that copy's to keep.
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$startup = (Get-ItemProperty -LiteralPath $runKey -Name 'Captastic' -ErrorAction SilentlyContinue).Captastic
if ($startup) {
    $installPrefix = $InstallDirectory.TrimEnd('\') + '\'
    if ($startup.Trim().Trim('"').StartsWith($installPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        Remove-ItemProperty -LiteralPath $runKey -Name 'Captastic'
    }
}

$shortcutPath = Join-Path ([Environment]::GetFolderPath('Programs')) 'Captastic.lnk'
if (Test-Path -LiteralPath $shortcutPath) {
    Remove-Item -LiteralPath $shortcutPath -Force
}

Set-Location -LiteralPath ([System.IO.Path]::GetTempPath())
if (Test-Path -LiteralPath $InstallDirectory -PathType Container) {
    Remove-Item -LiteralPath $InstallDirectory -Recurse -Force
}

if ($RemoveSettings) {
    $settingsDirectory = Join-Path $env:USERPROFILE '.captastic'
    if (Test-Path -LiteralPath $settingsDirectory -PathType Container) {
        Remove-Item -LiteralPath $settingsDirectory -Recurse -Force
    }
}

Write-Host 'Captastic was uninstalled.'
if (-not $RemoveSettings) {
    Write-Host 'Configuration and logs were retained under ~/.captastic.'
}
