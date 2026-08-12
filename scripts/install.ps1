[CmdletBinding()]
param(
    [string]$InstallDirectory = (Join-Path $env:LOCALAPPDATA 'Programs\Captastic'),
    [switch]$StartWithWindows,
    [switch]$NoLaunch
)

$ErrorActionPreference = 'Stop'
$packageDirectory = Split-Path -Parent $MyInvocation.MyCommand.Path
$sourceCli = Join-Path $packageDirectory 'captastic.exe'
$sourceDesktop = Join-Path $packageDirectory 'captastic-desktop.exe'

foreach ($requiredFile in @($sourceCli, $sourceDesktop)) {
    if (-not (Test-Path -LiteralPath $requiredFile -PathType Leaf)) {
        throw "The release package is incomplete: $requiredFile is missing."
    }
}

$InstallDirectory = [System.IO.Path]::GetFullPath($InstallDirectory)
$installRoot = [System.IO.Path]::GetPathRoot($InstallDirectory)
if ($InstallDirectory.TrimEnd('\') -eq $installRoot.TrimEnd('\')) {
    throw 'The installation directory cannot be a filesystem root.'
}
$installedCli = Join-Path $InstallDirectory 'captastic.exe'
$installedDesktop = Join-Path $InstallDirectory 'captastic-desktop.exe'

if (Test-Path -LiteralPath $installedCli -PathType Leaf) {
    & $installedCli stop | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "The installed Captastic daemon stop command failed with exit code $LASTEXITCODE."
    }
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        Start-Sleep -Milliseconds 100
        $statusJson = & $installedCli status --json
        if ($LASTEXITCODE -ne 0) {
            throw "The installed Captastic status command failed with exit code $LASTEXITCODE."
        }
        $status = ($statusJson | ConvertFrom-Json).status
    } while ($status -eq 'running' -and [DateTime]::UtcNow -lt $deadline)
    if ($status -eq 'running') {
        throw 'The running Captastic daemon did not stop within five seconds.'
    }
    if ($status -ne 'not_running') {
        throw "The installed Captastic daemon returned unexpected status '$status'; refusing to replace its files."
    }
}

New-Item -ItemType Directory -Path $InstallDirectory -Force | Out-Null
foreach ($name in @(
    'captastic.exe',
    'captastic-desktop.exe',
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
$shortcut.TargetPath = $installedDesktop
$shortcut.WorkingDirectory = $InstallDirectory
$shortcut.IconLocation = "$installedDesktop,0"
$shortcut.Description = 'Start Captastic screenshot capture'
$shortcut.Save()

if ($StartWithWindows) {
    & $installedCli startup enable
    if ($LASTEXITCODE -ne 0) {
        throw "Captastic startup registration failed with exit code $LASTEXITCODE."
    }
}

if (-not $NoLaunch) {
    Start-Process -FilePath $installedDesktop -WorkingDirectory $InstallDirectory -WindowStyle Hidden
}

Write-Host "Captastic installed to $InstallDirectory"
Write-Host "Start Menu shortcut: $shortcutPath"
