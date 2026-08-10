[CmdletBinding()]
param(
    [string]$InstallDirectory = (Join-Path $env:LOCALAPPDATA 'Programs\Captastic'),
    [switch]$RemoveSettings
)

$ErrorActionPreference = 'Stop'
$InstallDirectory = [System.IO.Path]::GetFullPath($InstallDirectory)
$installRoot = [System.IO.Path]::GetPathRoot($InstallDirectory)
if ($InstallDirectory.TrimEnd('\') -eq $installRoot.TrimEnd('\')) {
    throw 'The installation directory cannot be a filesystem root.'
}
$installedCli = Join-Path $InstallDirectory 'captastic.exe'

if (Test-Path -LiteralPath $installedCli -PathType Leaf) {
    & $installedCli stop | Out-Null
    & $installedCli startup disable | Out-Null
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
