#Requires -Version 7.0

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Version,
    [Parameter(Mandatory)]
    [string]$ArchivePath,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '..\dist'),
    [string]$SourceUrl
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$packageSource = Join-Path $repositoryRoot 'packaging\chocolatey'
$ArchivePath = [System.IO.Path]::GetFullPath($ArchivePath)
$OutputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)
$packageVersion = $Version.TrimStart('v')

if ($packageVersion -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$') {
    throw "Chocolatey package version '$packageVersion' is not a valid SemVer version."
}
if (-not (Test-Path -LiteralPath $ArchivePath -PathType Leaf)) {
    throw "Portable release archive is missing: $ArchivePath"
}
if (-not (Get-Command choco -ErrorAction SilentlyContinue)) {
    throw 'Chocolatey is required to build the package. Install it from https://chocolatey.org/install.'
}
if ([string]::IsNullOrWhiteSpace($SourceUrl)) {
    $archiveName = Split-Path -Leaf $ArchivePath
    $SourceUrl = "https://github.com/stevenpickles/captastic/releases/download/v$packageVersion/$archiveName"
}

New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$stagingDirectory = Join-Path $OutputDirectory ".captastic-chocolatey-$([Guid]::NewGuid().ToString('N'))"
$expandedDirectory = Join-Path $stagingDirectory 'expanded'
$toolsDirectory = Join-Path $stagingDirectory 'tools'
$applicationDirectory = Join-Path $toolsDirectory 'captastic'

try {
    New-Item -ItemType Directory -Path $expandedDirectory, $toolsDirectory, $applicationDirectory -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $packageSource 'captastic.nuspec') -Destination $stagingDirectory
    foreach ($script in @('chocolateyInstall.ps1', 'chocolateyBeforeModify.ps1', 'chocolateyUninstall.ps1')) {
        Copy-Item -LiteralPath (Join-Path $packageSource "tools\$script") -Destination $toolsDirectory
    }

    Expand-Archive -LiteralPath $ArchivePath -DestinationPath $expandedDirectory
    $payloadRoot = $expandedDirectory
    if (-not (Test-Path -LiteralPath (Join-Path $payloadRoot 'captastic.exe') -PathType Leaf)) {
        $candidateRoots = @(Get-ChildItem -LiteralPath $expandedDirectory -Directory)
        if ($candidateRoots.Count -ne 1 -or
            -not (Test-Path -LiteralPath (Join-Path $candidateRoots[0].FullName 'captastic.exe') -PathType Leaf)) {
            throw 'The portable archive must contain captastic.exe at its root or beneath one top-level directory.'
        }
        $payloadRoot = $candidateRoots[0].FullName
    }

    Copy-Item -Path (Join-Path $payloadRoot '*') -Destination $applicationDirectory -Recurse -Force
    foreach ($portableInstaller in @('install.ps1', 'uninstall.ps1')) {
        Remove-Item -LiteralPath (Join-Path $applicationDirectory $portableInstaller) -Force -ErrorAction SilentlyContinue
    }
    foreach ($binary in @('captastic.exe', 'captastic-desktop.exe')) {
        $binaryPath = Join-Path $applicationDirectory $binary
        if (-not (Test-Path -LiteralPath $binaryPath -PathType Leaf)) {
            throw "The portable archive is missing $binary."
        }
        New-Item -ItemType File -Path "$binaryPath.ignore" -Force | Out-Null
    }

    $verification = Get-Content -LiteralPath (Join-Path $packageSource 'tools\VERIFICATION.txt.template') -Raw
    $verification = $verification.Replace('{{SOURCE_URL}}', $SourceUrl)
    $verification = $verification.Replace(
        '{{ARCHIVE_SHA256}}',
        (Get-FileHash -Algorithm SHA256 -LiteralPath $ArchivePath).Hash.ToLowerInvariant()
    )
    $verification = $verification.Replace(
        '{{CLI_SHA256}}',
        (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $applicationDirectory 'captastic.exe')).Hash.ToLowerInvariant()
    )
    $verification = $verification.Replace(
        '{{DESKTOP_SHA256}}',
        (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $applicationDirectory 'captastic-desktop.exe')).Hash.ToLowerInvariant()
    )
    Set-Content -LiteralPath (Join-Path $toolsDirectory 'VERIFICATION.txt') -Value $verification -Encoding utf8

    $packOutput = & choco pack (Join-Path $stagingDirectory 'captastic.nuspec') `
        --version $packageVersion `
        --outputdirectory $OutputDirectory `
        --limit-output
    $packExitCode = $LASTEXITCODE
    $packOutput | ForEach-Object { Write-Host $_ }
    if ($packExitCode -ne 0) {
        throw "Chocolatey package creation failed with exit code $packExitCode."
    }
} finally {
    Remove-Item -LiteralPath $stagingDirectory -Recurse -Force -ErrorAction SilentlyContinue
}

$packagePath = Join-Path $OutputDirectory "captastic.$packageVersion.nupkg"
if (-not (Test-Path -LiteralPath $packagePath -PathType Leaf)) {
    throw "Chocolatey did not produce the expected package: $packagePath"
}

Write-Host "Chocolatey package: $packagePath"
Write-Output $packagePath
