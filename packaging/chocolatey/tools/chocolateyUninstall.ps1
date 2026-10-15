$ErrorActionPreference = 'Stop'

# Chocolatey runs chocolateyBeforeModify.ps1 first, which stops a running daemon and fails the
# uninstall if it cannot. What is left here is what only an uninstall removes.

# Launch at login, if it was turned on. The entry is per-user and names this package's executable.
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
if ($null -ne (Get-ItemProperty -LiteralPath $runKey -Name 'Captastic' -ErrorAction SilentlyContinue)) {
    Remove-ItemProperty -LiteralPath $runKey -Name 'Captastic'
}

# The before-modify hook leaves this for an upgrade's install; an uninstall has no install to read it.
Remove-Item -LiteralPath (Join-Path $env:TEMP 'captastic-relaunch-after-upgrade') -Force -ErrorAction SilentlyContinue

Uninstall-BinFile -Name 'captastic'
# Created by 0.2 and earlier, and removed here in case an upgrade's own clean-up did not run.
Uninstall-BinFile -Name 'captastic-desktop'

$shortcutPath = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'Captastic.lnk'
if (Test-Path -LiteralPath $shortcutPath) {
    Remove-Item -LiteralPath $shortcutPath -Force
}

Write-Host 'Captastic was uninstalled.'
Write-Host 'Configuration and logs were retained under ~/.captastic.'
