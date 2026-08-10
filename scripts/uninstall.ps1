[CmdletBinding()]
param(
    [string]$InstallDirectory = (Join-Path $env:LOCALAPPDATA 'Programs\Captastic'),
    [switch]$RemoveSettings
)

$ErrorActionPreference = 'Stop'

function Wait-CaptasticStopped {
    param(
        [Parameter(Mandatory)]
        [string]$Executable,
        [int]$TimeoutSeconds = 5
    )

    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    while ($true) {
        $statusResult = & $Executable status --json | ConvertFrom-Json
        if ($LASTEXITCODE -ne 0) {
            throw "Captastic status failed with exit code $LASTEXITCODE."
        }
        switch ($statusResult.status) {
            'stopped' {
                return
            }
            'running' {
                if ([DateTime]::UtcNow -ge $deadline) {
                    throw "The running Captastic daemon did not stop within $TimeoutSeconds seconds."
                }
                Start-Sleep -Milliseconds 100
            }
            default {
                throw "Captastic returned an unexpected daemon status: $($statusResult.status)"
            }
        }
    }
}

$InstallDirectory = [System.IO.Path]::GetFullPath($InstallDirectory)
$installRoot = [System.IO.Path]::GetPathRoot($InstallDirectory)
if ($InstallDirectory.TrimEnd('\') -eq $installRoot.TrimEnd('\')) {
    throw 'The installation directory cannot be a filesystem root.'
}
$installedCli = Join-Path $InstallDirectory 'captastic.exe'

if (Test-Path -LiteralPath $installedCli -PathType Leaf) {
    & $installedCli stop | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "Captastic stop failed with exit code $LASTEXITCODE."
    }
    Wait-CaptasticStopped -Executable $installedCli
    & $installedCli startup disable | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "Disabling Captastic startup failed with exit code $LASTEXITCODE."
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
