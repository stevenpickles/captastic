#Requires -Version 7.0

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$ManifestPath
)

$ErrorActionPreference = 'Stop'
$ManifestPath = [System.IO.Path]::GetFullPath($ManifestPath)
if (-not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) {
    throw "Artifact manifest is missing: $ManifestPath"
}
$manifest = Get-Content -LiteralPath $ManifestPath -Raw | ConvertFrom-Json
if ($manifest.schemaVersion -ne 1) {
    throw "Unsupported artifact manifest schema version '$($manifest.schemaVersion)'."
}
if ($manifest.architecture -notin @('x86_64', 'arm64')) {
    throw "Unsupported artifact manifest architecture '$($manifest.architecture)'."
}
$expectedKinds = if ($manifest.architecture -eq 'x86_64') {
    @('portable-archive', 'portable-checksum', 'chocolatey-package')
} else {
    @('portable-archive', 'portable-checksum')
}
$artifacts = @($manifest.artifacts)
if ($artifacts.Count -ne $expectedKinds.Count) {
    throw "Artifact manifest contains $($artifacts.Count) artifacts; expected $($expectedKinds.Count)."
}
$manifestDirectory = Split-Path -Parent $ManifestPath
$seenFiles = @{}
foreach ($kind in $expectedKinds) {
    $matchingArtifacts = @($artifacts | Where-Object { $_.kind -eq $kind })
    if ($matchingArtifacts.Count -ne 1) {
        throw "Artifact manifest must contain exactly one $kind entry."
    }
}
foreach ($artifact in $artifacts) {
    $file = [string]$artifact.file
    if ([string]::IsNullOrWhiteSpace($file) -or $file -ne (Split-Path -Leaf $file)) {
        throw "Artifact manifest file must be a leaf filename: $file"
    }
    if ($seenFiles.ContainsKey($file)) {
        throw "Artifact manifest contains duplicate filename $file."
    }
    $seenFiles[$file] = $true
    $artifactPath = Join-Path $manifestDirectory $file
    if (-not (Test-Path -LiteralPath $artifactPath -PathType Leaf)) {
        throw "Artifact listed by the manifest is missing: $artifactPath"
    }
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $artifactPath).Hash.ToLowerInvariant()
    if ($hash -ne $artifact.sha256) {
        throw "Artifact hash does not match the manifest: $artifactPath"
    }
}
if ($manifest.architecture -eq 'x86_64') {
    $sourceUri = $null
    if (-not [System.Uri]::TryCreate(
        [string]$manifest.sourceArchiveUrl,
        [System.UriKind]::Absolute,
        [ref]$sourceUri
    ) -or $sourceUri.Scheme -ne 'https') {
        throw 'The x86_64 artifact manifest must contain an absolute HTTPS source archive URL.'
    }
}

Write-Host "Artifact manifest validation passed: $ManifestPath"
Write-Output $manifest
