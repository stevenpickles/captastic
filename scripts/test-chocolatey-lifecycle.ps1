#Requires -Version 7.0

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$InitialManifestPath,
    [Parameter(Mandatory)]
    [string]$UpgradeManifestPath,
    [Parameter(Mandatory)]
    [switch]$AllowSystemChanges
)

$ErrorActionPreference = 'Stop'
if (-not $AllowSystemChanges) {
    throw 'Chocolatey lifecycle testing changes the installed package state and requires -AllowSystemChanges.'
}
if (-not (Get-Command choco -ErrorAction SilentlyContinue)) {
    throw 'Chocolatey is required for lifecycle testing.'
}

function Read-PackageManifest([string]$Path) {
    $fullPath = [System.IO.Path]::GetFullPath($Path)
    if (-not (Test-Path -LiteralPath $fullPath -PathType Leaf)) {
        throw "Artifact manifest is missing: $fullPath"
    }
    $manifest = Get-Content -LiteralPath $fullPath -Raw | ConvertFrom-Json
    if ($manifest.architecture -ne 'x86_64') {
        throw "Chocolatey lifecycle testing requires x86_64 artifacts; received $($manifest.architecture)."
    }
    $packageArtifact = @($manifest.artifacts | Where-Object { $_.kind -eq 'chocolatey-package' })
    if ($packageArtifact.Count -ne 1) {
        throw "Artifact manifest must contain exactly one Chocolatey package: $fullPath"
    }
    $packagePath = Join-Path (Split-Path -Parent $fullPath) $packageArtifact[0].file
    if (-not (Test-Path -LiteralPath $packagePath -PathType Leaf)) {
        throw "Chocolatey package listed by the manifest is missing: $packagePath"
    }
    $packageHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $packagePath).Hash.ToLowerInvariant()
    if ($packageHash -ne $packageArtifact[0].sha256) {
        throw "Chocolatey package hash does not match its artifact manifest: $packagePath"
    }
    return [PSCustomObject]@{
        Manifest = $manifest
        Directory = Split-Path -Parent $fullPath
        PackagePath = $packagePath
    }
}

function Invoke-Choco([string[]]$Arguments) {
    & choco @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "choco $($Arguments -join ' ') failed with exit code $LASTEXITCODE."
    }
}

$initial = Read-PackageManifest $InitialManifestPath
$upgrade = Read-PackageManifest $UpgradeManifestPath
if ([System.Management.Automation.SemanticVersion]$upgrade.Manifest.version -le
    [System.Management.Automation.SemanticVersion]$initial.Manifest.version) {
    throw 'The lifecycle upgrade version must be greater than the initial version.'
}
$configurationDirectory = Join-Path ([Environment]::GetFolderPath('UserProfile')) '.captastic'
$markerPath = Join-Path $configurationDirectory 'chocolatey-lifecycle-test.marker'
if (Test-Path -LiteralPath $configurationDirectory) {
    throw "Lifecycle testing refuses to use an existing Captastic configuration directory: $configurationDirectory"
}
$chocolateyBin = Join-Path $env:ChocolateyInstall 'bin'
$shortcutPath = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'Captastic.lnk'
$installed = $false

try {
    New-Item -ItemType Directory -Path $configurationDirectory -Force | Out-Null
    [System.IO.File]::WriteAllText($markerPath, 'preserve-me')

    $installed = $true
    Invoke-Choco @(
        'install', 'captastic',
        '--version', [string]$initial.Manifest.version,
        '--source', $initial.Directory,
        '--yes', '--pre', '--no-progress', '--limit-output'
    )
    foreach ($shim in @('captastic.exe', 'captastic-desktop.exe')) {
        if (-not (Test-Path -LiteralPath (Join-Path $chocolateyBin $shim) -PathType Leaf)) {
            throw "Chocolatey install did not create shim $shim."
        }
    }
    if (-not (Test-Path -LiteralPath $shortcutPath -PathType Leaf)) {
        throw "Chocolatey install did not create Start Menu shortcut $shortcutPath."
    }
    $status = & (Join-Path $chocolateyBin 'captastic.exe') status --json | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0 -or $status.status -ne 'not_running') {
        throw 'Chocolatey first install unexpectedly launched Captastic.'
    }

    Invoke-Choco @(
        'upgrade', 'captastic',
        '--version', [string]$upgrade.Manifest.version,
        '--source', $upgrade.Directory,
        '--yes', '--pre', '--no-progress', '--limit-output'
    )
    Invoke-Choco @('uninstall', 'captastic', '--yes', '--no-progress', '--limit-output')
    $installed = $false

    foreach ($shim in @('captastic.exe', 'captastic-desktop.exe')) {
        if (Test-Path -LiteralPath (Join-Path $chocolateyBin $shim)) {
            throw "Chocolatey uninstall did not remove shim $shim."
        }
    }
    if (Test-Path -LiteralPath $shortcutPath) {
        throw 'Chocolatey uninstall did not remove the Start Menu shortcut.'
    }
    if (-not (Test-Path -LiteralPath $markerPath -PathType Leaf) -or
        (Get-Content -LiteralPath $markerPath -Raw) -ne 'preserve-me') {
        throw 'Chocolatey uninstall did not preserve the Captastic settings fixture.'
    }
    Write-Host 'Chocolatey install, upgrade, and uninstall lifecycle tests passed.'
} finally {
    if ($installed) {
        & choco uninstall captastic --yes --no-progress --limit-output | Out-Host
    }
    Remove-Item -LiteralPath $configurationDirectory -Recurse -Force -ErrorAction SilentlyContinue
}
