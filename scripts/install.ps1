[CmdletBinding()]
param(
    [string]$InstallDirectory = (Join-Path $env:LOCALAPPDATA 'Programs\Captastic'),
    [switch]$StartWithWindows,
    [switch]$NoLaunch
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

$packageDirectory = Split-Path -Parent $MyInvocation.MyCommand.Path
$sourceExecutable = Join-Path $packageDirectory 'captastic.exe'

if (-not (Test-Path -LiteralPath $sourceExecutable -PathType Leaf)) {
    throw "The release package is incomplete: $sourceExecutable is missing."
}

$InstallDirectory = [System.IO.Path]::GetFullPath($InstallDirectory)
$installRoot = [System.IO.Path]::GetPathRoot($InstallDirectory)
if ($InstallDirectory.TrimEnd('\') -eq $installRoot.TrimEnd('\')) {
    throw 'The installation directory cannot be a filesystem root.'
}
$installedExecutable = Join-Path $InstallDirectory 'captastic.exe'
$legacyLauncher = Join-Path $InstallDirectory 'captastic-desktop.exe'

# Only an upgrade has a daemon whose files are about to be replaced.
if (Test-Path -LiteralPath $installedExecutable -PathType Leaf) {
    Stop-Captastic
}

New-Item -ItemType Directory -Path $InstallDirectory -Force | Out-Null
# 0.2 and earlier shipped a separate launcher; leaving it would leave a second way to start a
# version that no longer exists.
if (Test-Path -LiteralPath $legacyLauncher -PathType Leaf) {
    Remove-Item -LiteralPath $legacyLauncher -Force
}
foreach ($name in @(
    'captastic.exe',
    'captastic.example.toml',
    'README.md',
    'LICENSE-MIT',
    'LICENSE-APACHE',
    'uninstall.ps1'
)) {
    $source = Join-Path $packageDirectory $name
    if (Test-Path -LiteralPath $source -PathType Leaf) {
        Copy-Item -LiteralPath $source -Destination (Join-Path $InstallDirectory $name) -Force
    }
}

$programsDirectory = [Environment]::GetFolderPath('Programs')
$shortcutPath = Join-Path $programsDirectory 'Captastic.lnk'
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($shortcutPath)
$shortcut.TargetPath = $installedExecutable
$shortcut.WorkingDirectory = $InstallDirectory
$shortcut.IconLocation = "$installedExecutable,0"
$shortcut.Description = 'Start Captastic screenshot capture'
$shortcut.Save()

# Launch at login names the one executable. An entry 0.2 or earlier left for this directory names
# its retired launcher, which the clean-up above has just deleted, so it is repointed as well.
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$startupValue = '"{0}"' -f $installedExecutable
$existingStartup = (Get-ItemProperty -LiteralPath $runKey -Name 'Captastic' -ErrorAction SilentlyContinue).Captastic
$legacyStartup = $existingStartup -and
    $existingStartup.Trim().Trim('"').Equals($legacyLauncher, [System.StringComparison]::OrdinalIgnoreCase)
if ($StartWithWindows -or $legacyStartup) {
    if (-not (Test-Path -LiteralPath $runKey)) {
        New-Item -Path $runKey | Out-Null
    }
    Set-ItemProperty -LiteralPath $runKey -Name 'Captastic' -Value $startupValue
}

if (-not $NoLaunch) {
    Start-Process -FilePath $installedExecutable -WorkingDirectory $InstallDirectory
}

Write-Host "Captastic installed to $InstallDirectory"
Write-Host "Start Menu shortcut: $shortcutPath"
