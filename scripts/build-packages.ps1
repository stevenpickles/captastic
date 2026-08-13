#Requires -Version 7.0

[CmdletBinding()]
param(
    [string]$Version,
    [string]$PrereleaseLabel,
    [string]$ReleaseTag,
    [ValidateSet('x86_64', 'arm64')]
    [string]$Architecture = 'x86_64',
    [string]$BinariesDirectory,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '..\dist'),
    [string]$SourceUrl,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$OutputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)
$portableScript = Join-Path $PSScriptRoot 'package-windows.ps1'
$chocolateyScript = Join-Path $PSScriptRoot 'package-chocolatey.ps1'
$manifestPath = Join-Path $OutputDirectory 'artifacts.json'

$metadataOutput = & cargo metadata --locked --no-deps --format-version 1
if ($LASTEXITCODE -ne 0) {
    throw "cargo metadata failed with exit code $LASTEXITCODE."
}
$metadata = $metadataOutput | ConvertFrom-Json
$package = @($metadata.packages | Where-Object {
    @($_.targets | Where-Object { $_.name -eq 'captastic' -and $_.kind -contains 'bin' }).Count -gt 0
})
if ($package.Count -ne 1) {
    throw "Expected exactly one Cargo package containing the captastic binary target; found $($package.Count)."
}
$workspaceVersion = [string]$package[0].version
$explicitVersion = -not [string]::IsNullOrWhiteSpace($Version)
$explicitPrerelease = -not [string]::IsNullOrWhiteSpace($PrereleaseLabel)
if ($explicitVersion -and $explicitPrerelease) {
    throw 'Specify either Version or PrereleaseLabel, not both.'
}
$packageVersion = if ($explicitPrerelease) {
    "$workspaceVersion-$PrereleaseLabel"
} elseif (-not $explicitVersion) {
    $workspaceVersion
} else {
    $Version.TrimStart('v')
}
if ($packageVersion -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$') {
    throw "Package version '$packageVersion' is not a valid SemVer version."
}
if (($packageVersion -split '-', 2)[0] -ne $workspaceVersion) {
    throw "Package version $packageVersion must have workspace version $workspaceVersion as its release core."
}
if (-not [string]::IsNullOrWhiteSpace($ReleaseTag)) {
    $expectedTag = "v$workspaceVersion"
    if ($ReleaseTag -ne $expectedTag -or $packageVersion -ne $workspaceVersion) {
        throw "Release tag $ReleaseTag must exactly match $expectedTag and package version $workspaceVersion."
    }
}
if ($Architecture -eq 'x86_64' -and
    $packageVersion -ne $workspaceVersion -and
    [string]::IsNullOrWhiteSpace($SourceUrl)) {
    throw 'Prerelease Chocolatey packages require an explicit source archive URL.'
}

$rustTarget = if ($Architecture -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
if ([string]::IsNullOrWhiteSpace($BinariesDirectory)) {
    $BinariesDirectory = if ($Architecture -eq 'arm64') {
        Join-Path $repositoryRoot 'target\aarch64-pc-windows-msvc\dist'
    } else {
        Join-Path $repositoryRoot 'target\dist'
    }
}
$BinariesDirectory = [System.IO.Path]::GetFullPath($BinariesDirectory)

if (-not $SkipBuild) {
    $cargoArguments = @('build', '--locked', '--profile', 'dist', '--workspace')
    if ($Architecture -eq 'arm64') {
        $cargoArguments += @('--target', $rustTarget)
    }
    & cargo @cargoArguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE."
    }
}

New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
Remove-Item -LiteralPath $manifestPath -Force -ErrorAction SilentlyContinue
$archive = & $portableScript `
    -Version $packageVersion `
    -Architecture $Architecture `
    -BinariesDirectory $BinariesDirectory `
    -OutputDirectory $OutputDirectory |
    Select-Object -Last 1
if (-not (Test-Path -LiteralPath $archive -PathType Leaf)) {
    throw "Portable packaging did not produce the expected archive: $archive"
}

$publishable = -not [string]::IsNullOrWhiteSpace($ReleaseTag)
$packagePath = $null
$chocolateyVersion = $null
if ($Architecture -eq 'x86_64') {
    if ([string]::IsNullOrWhiteSpace($SourceUrl)) {
        $archiveName = Split-Path -Leaf $archive
        $SourceUrl = "https://github.com/stevenpickles/captastic/releases/download/v$workspaceVersion/$archiveName"
    }
    $packagePath = & $chocolateyScript `
        -Version $packageVersion `
        -Architecture $Architecture `
        -ArchivePath $archive `
        -OutputDirectory $OutputDirectory `
        -SourceUrl $SourceUrl `
        -Publishable:$publishable |
        Select-Object -Last 1
    if (-not (Test-Path -LiteralPath $packagePath -PathType Leaf)) {
        throw "Chocolatey packaging did not produce the expected package: $packagePath"
    }
    $chocolateyVersion = (& choco --version | Select-Object -First 1).Trim()
} else {
    Write-Host 'Chocolatey packaging is currently restricted to x86_64; only the ARM64 portable archive was built.'
}

$artifactItems = @(
    [ordered]@{
        kind = 'portable-archive'
        file = Split-Path -Leaf $archive
        sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant()
    },
    [ordered]@{
        kind = 'portable-checksum'
        file = Split-Path -Leaf "$archive.sha256"
        sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath "$archive.sha256").Hash.ToLowerInvariant()
    }
)
if ($null -ne $packagePath) {
    $artifactItems += [ordered]@{
        kind = 'chocolatey-package'
        file = Split-Path -Leaf $packagePath
        sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $packagePath).Hash.ToLowerInvariant()
    }
}

$manifest = [ordered]@{
    schemaVersion = 1
    version = $packageVersion
    workspaceVersion = $workspaceVersion
    architecture = $Architecture
    rustTarget = $rustTarget
    cargoProfile = 'dist'
    publishable = $publishable
    sourceArchiveUrl = if ($Architecture -eq 'x86_64') { $SourceUrl } else { $null }
    tools = [ordered]@{
        chocolatey = $chocolateyVersion
    }
    artifacts = $artifactItems
}
$manifestJson = $manifest | ConvertTo-Json -Depth 6
[System.IO.File]::WriteAllText(
    $manifestPath,
    "$manifestJson`n",
    [System.Text.UTF8Encoding]::new($false)
)

Write-Host "Artifact manifest: $manifestPath"
[PSCustomObject]@{
    Version = $packageVersion
    Architecture = $Architecture
    ArchivePath = $archive
    ChecksumPath = "$archive.sha256"
    ChocolateyPackagePath = $packagePath
    ManifestPath = $manifestPath
}
