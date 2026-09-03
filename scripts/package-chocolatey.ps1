#Requires -Version 7.0

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Version,
    [string]$Architecture = 'x86_64',
    [Parameter(Mandatory)]
    [string]$ArchivePath,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '..\dist'),
    [string]$SourceUrl,
    [switch]$Publishable
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$packageSource = Join-Path $repositoryRoot 'packaging\chocolatey'
$ArchivePath = [System.IO.Path]::GetFullPath($ArchivePath)
$OutputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)
$packageVersion = $Version.TrimStart('v')
$packagePath = Join-Path $OutputDirectory "captastic.$packageVersion.nupkg"

if ($Architecture -ne 'x86_64') {
    throw "Chocolatey packaging currently supports only x86_64; received $Architecture."
}
if ($packageVersion -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$') {
    throw "Chocolatey package version '$packageVersion' is not a valid SemVer version."
}
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
Remove-Item -LiteralPath $packagePath -Force -ErrorAction SilentlyContinue
if (-not (Test-Path -LiteralPath $ArchivePath -PathType Leaf)) {
    throw "Portable release archive is missing: $ArchivePath"
}
$expectedArchiveName = "captastic-$packageVersion-windows-x86_64.zip"
if ((Split-Path -Leaf $ArchivePath) -ne $expectedArchiveName) {
    throw "Portable release archive must be named $expectedArchiveName."
}
$checksumPath = "$ArchivePath.sha256"
if (-not (Test-Path -LiteralPath $checksumPath -PathType Leaf)) {
    throw "Portable release checksum is missing: $checksumPath"
}
$checksumText = Get-Content -LiteralPath $checksumPath -Raw
$escapedArchiveName = [regex]::Escape($expectedArchiveName)
if ($checksumText -notmatch "^(?<hash>[0-9a-fA-F]{64})  $escapedArchiveName(?:`r?`n)?$") {
    throw "Portable release checksum has an invalid format: $checksumPath"
}
$expectedHash = $Matches.hash.ToLowerInvariant()
$archiveHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $ArchivePath).Hash.ToLowerInvariant()
if ($expectedHash -ne $archiveHash) {
    throw "Portable release checksum does not match $ArchivePath."
}
if (-not (Get-Command choco -ErrorAction SilentlyContinue)) {
    throw 'Chocolatey is required to build the package. Install it from https://chocolatey.org/install.'
}
if ([string]::IsNullOrWhiteSpace($SourceUrl)) {
    $archiveName = Split-Path -Leaf $ArchivePath
    $SourceUrl = "https://github.com/stevenpickles/captastic/releases/download/v$packageVersion/$archiveName"
}
$sourceUri = $null
if (-not [System.Uri]::TryCreate($SourceUrl, [System.UriKind]::Absolute, [ref]$sourceUri) -or
    $sourceUri.Scheme -ne 'https') {
    throw "Chocolatey verification source URL must be an absolute HTTPS URL: $SourceUrl"
}
$expectedPublishableUrl = "https://github.com/stevenpickles/captastic/releases/download/v$packageVersion/$expectedArchiveName"
if ($Publishable -and $SourceUrl -ne $expectedPublishableUrl) {
    throw "Publishable Chocolatey package source URL must be $expectedPublishableUrl; received $SourceUrl"
}

$stagingDirectory = Join-Path $OutputDirectory ".captastic-chocolatey-$([Guid]::NewGuid().ToString('N'))"
$expandedDirectory = Join-Path $stagingDirectory 'expanded'
$toolsDirectory = Join-Path $stagingDirectory 'tools'
$applicationDirectory = Join-Path $toolsDirectory 'captastic'

try {
    New-Item -ItemType Directory -Path $expandedDirectory, $toolsDirectory, $applicationDirectory -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $packageSource 'captastic.nuspec') -Destination $stagingDirectory
    foreach ($packageFile in @(
        'chocolateyInstall.ps1',
        'chocolateyBeforeModify.ps1',
        'chocolateyUninstall.ps1',
        'LICENSE.txt'
    )) {
        Copy-Item -LiteralPath (Join-Path $packageSource "tools\$packageFile") -Destination $toolsDirectory
    }

    Expand-Archive -LiteralPath $ArchivePath -DestinationPath $expandedDirectory
    $payloadRoot = $expandedDirectory
    if (-not (Test-Path -LiteralPath (Join-Path $payloadRoot 'captastic.exe') -PathType Leaf)) {
        $candidateItems = @(Get-ChildItem -LiteralPath $expandedDirectory -Force)
        if ($candidateItems.Count -ne 1 -or
            -not $candidateItems[0].PSIsContainer -or
            -not (Test-Path -LiteralPath (Join-Path $candidateItems[0].FullName 'captastic.exe') -PathType Leaf)) {
            throw 'The portable archive must contain captastic.exe at its root or beneath one top-level directory.'
        }
        $payloadRoot = $candidateItems[0].FullName
    }
    $expectedPayloadNames = @(
        'captastic.exe',
        'captastic-desktop.exe',
        'captastic.example.toml',
        'README.md',
        'LICENSE-MIT',
        'LICENSE-APACHE',
        'install.ps1',
        'uninstall.ps1'
    )
    $payloadItems = @(Get-ChildItem -LiteralPath $payloadRoot -Force)
    foreach ($payloadItem in $payloadItems) {
        if ($payloadItem.PSIsContainer -or $expectedPayloadNames -notcontains $payloadItem.Name) {
            throw "The portable archive contains unexpected payload entry $($payloadItem.Name)."
        }
    }
    foreach ($expectedPayloadName in $expectedPayloadNames) {
        if (-not (Test-Path -LiteralPath (Join-Path $payloadRoot $expectedPayloadName) -PathType Leaf)) {
            throw "The portable archive is missing $expectedPayloadName."
        }
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

    function Get-PeMachine([string]$Path) {
        $stream = [System.IO.File]::OpenRead($Path)
        $reader = [System.IO.BinaryReader]::new($stream)
        try {
            if ($stream.Length -lt 0x40) {
                throw "$Path is a truncated PE executable (the DOS header is incomplete)."
            }
            if ($reader.ReadUInt16() -ne 0x5A4D) {
                throw "$Path is not a PE executable (missing MZ header)."
            }
            $stream.Position = 0x3C
            $peOffset = $reader.ReadUInt32()
            if ($peOffset -gt ($stream.Length - 6)) {
                throw "$Path is a truncated PE executable (the PE header is outside the file)."
            }
            $stream.Position = $peOffset
            if ($reader.ReadUInt32() -ne 0x00004550) {
                throw "$Path is not a PE executable (missing PE signature)."
            }
            return $reader.ReadUInt16()
        } finally {
            $reader.Dispose()
            $stream.Dispose()
        }
    }
    foreach ($binary in @('captastic.exe', 'captastic-desktop.exe')) {
        $binaryPath = Join-Path $applicationDirectory $binary
        $machine = Get-PeMachine $binaryPath
        if ($machine -ne 0x8664) {
            throw ('Chocolatey payload {0} has PE machine 0x{1:X4}; expected x86_64 (0x8664).' -f `
                $binaryPath, $machine)
        }
    }

    $verification = Get-Content -LiteralPath (Join-Path $packageSource 'tools\VERIFICATION.txt.template') -Raw
    $verification = $verification.Replace('{{SOURCE_URL}}', $SourceUrl)
    $verification = $verification.Replace(
        '{{ARCHIVE_SHA256}}',
        $archiveHash
    )
    $verification = $verification.Replace(
        '{{CLI_SHA256}}',
        (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $applicationDirectory 'captastic.exe')).Hash.ToLowerInvariant()
    )
    $verification = $verification.Replace(
        '{{DESKTOP_SHA256}}',
        (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $applicationDirectory 'captastic-desktop.exe')).Hash.ToLowerInvariant()
    )
    if ($verification -match '\{\{[^}]+\}\}') {
        throw 'Chocolatey VERIFICATION.txt contains unresolved template placeholders.'
    }
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

if (-not (Test-Path -LiteralPath $packagePath -PathType Leaf)) {
    throw "Chocolatey did not produce the expected package: $packagePath"
}

Add-Type -AssemblyName System.IO.Compression.FileSystem
$packageArchive = [System.IO.Compression.ZipFile]::OpenRead($packagePath)
try {
    $entries = @($packageArchive.Entries | ForEach-Object { $_.FullName.Replace('\', '/') })
    foreach ($requiredEntry in @(
        'tools/LICENSE.txt',
        'tools/VERIFICATION.txt',
        'tools/chocolateyInstall.ps1',
        'tools/chocolateyBeforeModify.ps1',
        'tools/chocolateyUninstall.ps1',
        'tools/captastic/captastic.exe',
        'tools/captastic/captastic.exe.ignore',
        'tools/captastic/captastic-desktop.exe',
        'tools/captastic/captastic-desktop.exe.ignore',
        'tools/captastic/LICENSE-MIT',
        'tools/captastic/LICENSE-APACHE'
    )) {
        if ($entries -notcontains $requiredEntry) {
            throw "Generated Chocolatey package is missing $requiredEntry."
        }
    }
    foreach ($forbiddenEntry in @(
        'tools/captastic/install.ps1',
        'tools/captastic/uninstall.ps1'
    )) {
        if ($entries -contains $forbiddenEntry) {
            throw "Generated Chocolatey package unexpectedly contains $forbiddenEntry."
        }
    }
    $allowedEntryPatterns = @(
        '^_rels/\.rels$',
        '^captastic\.nuspec$',
        '^tools/(LICENSE\.txt|VERIFICATION\.txt|chocolatey(?:Install|BeforeModify|Uninstall)\.ps1)$',
        '^tools/captastic/(captastic(?:-desktop)?\.exe(?:\.ignore)?|captastic\.example\.toml|LICENSE-(?:MIT|APACHE)|README\.md)$',
        '^\[Content_Types\]\.xml$',
        '^package/services/metadata/core-properties/[0-9a-f]+\.psmdcp$'
    )
    foreach ($entry in $entries) {
        if (@($allowedEntryPatterns | Where-Object { $entry -match $_ }).Count -eq 0) {
            throw "Generated Chocolatey package contains unexpected entry $entry."
        }
    }
} finally {
    $packageArchive.Dispose()
}

Write-Host "Chocolatey package: $packagePath"
Write-Output $packagePath
