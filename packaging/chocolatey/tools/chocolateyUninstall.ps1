$ErrorActionPreference = 'Stop'

$toolsDirectory = Split-Path -Parent $MyInvocation.MyCommand.Definition
$cli = Join-Path $toolsDirectory 'captastic\captastic.exe'

if (Test-Path -LiteralPath $cli -PathType Leaf) {
    & $cli stop | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "Captastic stop returned exit code $LASTEXITCODE."
    }
    & $cli startup disable | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "Disabling Captastic startup returned exit code $LASTEXITCODE."
    }
}

Uninstall-BinFile -Name 'captastic'
Uninstall-BinFile -Name 'captastic-desktop'

$shortcutPath = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'Captastic.lnk'
if (Test-Path -LiteralPath $shortcutPath) {
    Remove-Item -LiteralPath $shortcutPath -Force
}

Write-Host 'Captastic was uninstalled.'
Write-Host 'Configuration and logs were retained under ~/.captastic.'
