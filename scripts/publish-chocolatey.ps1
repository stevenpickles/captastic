#Requires -Version 7.0
<#
.SYNOPSIS
Pushes a tagged release's Chocolatey package to the community repository, after checking the
published release it came from.

.DESCRIPTION
Run by .github/workflows/packages.yml, once the GitHub release for the tag is published.
Nothing is built here: the package pushed is the one attached to the release, downloaded through
the same direct URLs that VERIFICATION.txt names and that `choco install` will fetch from. Before
pushing, it checks that:

- the tag names a release version (vX.Y.Z, no prerelease label);
- the release's artifacts.json is publishable and is for that version;
- every file it lists downloads from its direct release URL with the SHA-256 it records.

The API key is read from the CHOCO_API_KEY environment variable, never from a parameter, so it
does not appear in a command line or a log. -DryRun does every check and stops short of the push;
it needs no key.

.PARAMETER Tag
The release tag, such as v0.5.0.

.PARAMETER Repository
The GitHub repository as owner/name.

.PARAMETER DryRun
Check the release and stop before pushing.

.PARAMETER OutputDirectory
Where to keep the checked package, so a workflow can attach it to its run.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Tag,
    [Parameter(Mandatory)]
    [string]$Repository,
    [switch]$DryRun,
    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

if ($Tag -notmatch '^v(?<version>\d+\.\d+\.\d+)$') {
    throw "$Tag is not a release tag (vX.Y.Z); only releases are pushed to Chocolatey."
}
$version = $Matches.version
$releaseUrl = "https://github.com/$Repository/releases/download/$Tag"

$directory = Join-Path ([System.IO.Path]::GetTempPath()) "captastic-chocolatey-$Tag-$PID"
New-Item -ItemType Directory -Path $directory -Force | Out-Null
try {
    $manifestPath = Join-Path $directory 'artifacts.json'
    Invoke-WebRequest -Uri "$releaseUrl/artifacts.json" -OutFile $manifestPath
    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    if ($manifest.publishable -ne $true) {
        throw "The $Tag release's artifacts.json is not marked publishable."
    }
    if ($manifest.version -ne $version) {
        throw "The $Tag release's artifacts.json is for version $($manifest.version), not $version."
    }

    $package = $null
    foreach ($artifact in @($manifest.artifacts)) {
        $file = [string]$artifact.file
        if ([string]::IsNullOrWhiteSpace($file) -or $file -ne (Split-Path -Leaf $file)) {
            throw "artifacts.json names a file that is not a plain file name: $file"
        }
        $path = Join-Path $directory $file
        Invoke-WebRequest -Uri "$releaseUrl/$file" -OutFile $path
        $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($hash -ne ([string]$artifact.sha256).ToLowerInvariant()) {
            throw "$file from the $Tag release has SHA-256 $hash, but artifacts.json records $($artifact.sha256)."
        }
        Write-Host "[ok] $file matches artifacts.json"
        if ($artifact.kind -eq 'chocolatey-package') {
            $package = $path
        }
    }
    if (-not $package) {
        throw "The $Tag release's artifacts.json lists no Chocolatey package."
    }
    $expected = "captastic.$version.nupkg"
    if ((Split-Path -Leaf $package) -ne $expected) {
        throw "The $Tag release's package is $(Split-Path -Leaf $package), not $expected."
    }

    if ($OutputDirectory) {
        New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
        Copy-Item -LiteralPath $package -Destination (Join-Path $OutputDirectory $expected) -Force
    }
    if ($DryRun) {
        Write-Host "Dry run: $expected from the $Tag release passed every check; not pushed."
        return
    }
    if ([string]::IsNullOrWhiteSpace($env:CHOCO_API_KEY)) {
        throw 'CHOCO_API_KEY is not set, so nothing was pushed.'
    }
    & choco push $package --source https://push.chocolatey.org/ --api-key $env:CHOCO_API_KEY --no-progress
    if ($LASTEXITCODE -ne 0) {
        throw "choco push failed with exit code $LASTEXITCODE."
    }
    Write-Host "Pushed $expected to the Chocolatey community repository; it now waits for moderation."
} finally {
    Remove-Item -LiteralPath $directory -Recurse -Force -ErrorAction SilentlyContinue
}
