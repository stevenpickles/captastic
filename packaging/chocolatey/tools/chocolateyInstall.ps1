$ErrorActionPreference = 'Stop'

$toolsDirectory = Split-Path -Parent $MyInvocation.MyCommand.Definition
$applicationDirectory = Join-Path $toolsDirectory 'captastic'
$cli = Join-Path $applicationDirectory 'captastic.exe'
$desktop = Join-Path $applicationDirectory 'captastic-desktop.exe'

foreach ($requiredFile in @($cli, $desktop)) {
    if (-not (Test-Path -LiteralPath $requiredFile -PathType Leaf)) {
        throw "The Captastic Chocolatey package is incomplete: $requiredFile is missing."
    }
    Unblock-File -LiteralPath $requiredFile
}

# A user moving from the portable installer should not be left with an old daemon
# holding Captastic's session control event. Its uninstaller preserves ~/.captastic.
$legacyDirectory = Join-Path $env:LOCALAPPDATA 'Programs\Captastic'
$legacyCli = Join-Path $legacyDirectory 'captastic.exe'
$legacyUninstaller = Join-Path $legacyDirectory 'uninstall.ps1'
if ((Test-Path -LiteralPath $legacyCli -PathType Leaf) -and
    -not [System.IO.Path]::GetFullPath($legacyCli).Equals(
        [System.IO.Path]::GetFullPath($cli),
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

# Suppression files in the generated package prevent Chocolatey's automatic shim
# behavior; only the console CLI should be exposed on PATH.
Install-BinFile -Name 'captastic' -Path $cli

$shortcutPath = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'Captastic.lnk'
Install-ChocolateyShortcut `
    -ShortcutFilePath $shortcutPath `
    -TargetPath $desktop `
    -WorkingDirectory $applicationDirectory `
    -IconLocation $desktop `
    -Description 'Start Captastic screenshot capture'


Write-Host 'Captastic is installed. Start it from the Start Menu.'
Write-Host 'Configuration and logs are stored under ~/.captastic.'
