#Requires -Version 7.0

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repositoryRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$portableScript = Join-Path $PSScriptRoot 'package-windows.ps1'
$chocolateyScript = Join-Path $PSScriptRoot 'package-chocolatey.ps1'
$orchestrationScript = Join-Path $PSScriptRoot 'build-packages.ps1'
$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) "captastic-package-tests-$PID"

function Write-TestPe([string]$Path, [uint16]$Machine) {
    $bytes = [byte[]]::new(0x88)
    [System.BitConverter]::GetBytes([uint16]0x5A4D).CopyTo($bytes, 0)
    [System.BitConverter]::GetBytes([uint32]0x80).CopyTo($bytes, 0x3C)
    [System.BitConverter]::GetBytes([uint32]0x00004550).CopyTo($bytes, 0x80)
    [System.BitConverter]::GetBytes($Machine).CopyTo($bytes, 0x84)
    [System.IO.File]::WriteAllBytes($Path, $bytes)
}

function New-TestBinaries([string]$Name, [uint16]$Machine) {
    $directory = Join-Path $testRoot $Name
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
    Write-TestPe (Join-Path $directory 'captastic.exe') $Machine
    Write-TestPe (Join-Path $directory 'captastic-desktop.exe') $Machine
    return $directory
}

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) {
        throw $Message
    }
}

function Assert-ThrowsLike([scriptblock]$Action, [string]$Pattern) {
    try {
        & $Action
    } catch {
        if ($_.Exception.Message -notlike $Pattern) {
            throw "Expected error like '$Pattern', received: $($_.Exception.Message)"
        }
        return
    }
    throw "Expected action to fail with '$Pattern'."
}

function Get-ZipEntryNames([string]$Path) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($Path)
    try {
        return @($zip.Entries | ForEach-Object { $_.FullName.Replace('\', '/') })
    } finally {
        $zip.Dispose()
    }
}

function Get-ZipEntryText([string]$Path, [string]$EntryName) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $entry = $zip.GetEntry($EntryName)
        if ($null -eq $entry) {
            throw "$Path does not contain $EntryName."
        }
        $reader = [System.IO.StreamReader]::new($entry.Open())
        try {
            return $reader.ReadToEnd()
        } finally {
            $reader.Dispose()
        }
    } finally {
        $zip.Dispose()
    }
}

try {
    New-Item -ItemType Directory -Path $testRoot -Force | Out-Null

    $x64 = New-TestBinaries 'x64' 0x8664
    $x64Output = Join-Path $testRoot 'x64-output'
    $x64Archive = & $portableScript -Version '0.0.0-test-x64' -Architecture x86_64 `
        -BinariesDirectory $x64 -OutputDirectory $x64Output |
        Select-Object -Last 1
    Assert-True (Test-Path -LiteralPath $x64Archive -PathType Leaf) `
        'x86_64 package test did not create an archive.'
    Assert-True (Test-Path -LiteralPath "$x64Archive.sha256" -PathType Leaf) `
        'x86_64 package test did not create a checksum.'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $x64Output 'captastic-0.0.0-test-x64-windows-x86_64'))) `
        'Portable package staging directory was not cleaned up.'

    $arm64 = New-TestBinaries 'arm64' 0xAA64
    $arm64Archive = & $portableScript -Version '0.0.0-test-arm64' -Architecture arm64 `
        -BinariesDirectory $arm64 -OutputDirectory (Join-Path $testRoot 'arm64-output') |
        Select-Object -Last 1
    Assert-True (Test-Path -LiteralPath $arm64Archive -PathType Leaf) `
        'ARM64 package test did not create an archive.'

    Assert-ThrowsLike {
        & $portableScript -Version '0.0.0-test-mismatch' -Architecture arm64 `
            -BinariesDirectory $x64 -OutputDirectory (Join-Path $testRoot 'mismatch-output')
    } '*has PE machine 0x8664; expected 0xAA64 for arm64*'

    $truncated = New-TestBinaries 'truncated' 0x8664
    [System.IO.File]::WriteAllBytes(
        (Join-Path $truncated 'captastic.exe'),
        [byte[]](0x4D, 0x5A)
    )
    Assert-ThrowsLike {
        & $portableScript -Version '0.0.0-test-truncated' -Architecture x86_64 `
            -BinariesDirectory $truncated -OutputDirectory (Join-Path $testRoot 'truncated-output')
    } '*truncated PE executable*DOS header is incomplete*'
    Assert-ThrowsLike {
        & $portableScript -Version '../invalid' -Architecture x86_64 `
            -BinariesDirectory $x64 -OutputDirectory (Join-Path $testRoot 'invalid-version-output')
    } '*not a valid SemVer version*'

    $chocolateyOutput = Join-Path $testRoot 'chocolatey-output'
    $chocolateyArchive = & $portableScript -Version '0.0.0-test-choco' -Architecture x86_64 `
        -BinariesDirectory $x64 -OutputDirectory $chocolateyOutput |
        Select-Object -Last 1
    $sourceUrl = 'https://example.invalid/captastic-0.0.0-test-choco-windows-x86_64.zip'
    $packagePath = & $chocolateyScript -Version '0.0.0-test-choco' -Architecture x86_64 `
        -ArchivePath $chocolateyArchive -OutputDirectory $chocolateyOutput -SourceUrl $sourceUrl |
        Select-Object -Last 1
    Assert-True (Test-Path -LiteralPath $packagePath -PathType Leaf) `
        'Chocolatey package test did not create a package.'
    Assert-True (@(Get-ChildItem -LiteralPath $chocolateyOutput -Directory -Filter '.captastic-chocolatey-*').Count -eq 0) `
        'Chocolatey staging directory was not cleaned up.'

    $entries = Get-ZipEntryNames $packagePath
    $requiredEntries = @(
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
        'tools/captastic/LICENSE-APACHE',
        'tools/captastic/README.md',
        'tools/captastic/captastic.example.toml'
    )
    foreach ($entry in $requiredEntries) {
        Assert-True ($entries -contains $entry) "Chocolatey package is missing $entry."
    }
    foreach ($entry in @('tools/captastic/install.ps1', 'tools/captastic/uninstall.ps1')) {
        Assert-True ($entries -notcontains $entry) "Chocolatey package unexpectedly contains $entry."
    }
    $allowedPatterns = @(
        '^_rels/\.rels$',
        '^captastic\.nuspec$',
        '^tools/(LICENSE\.txt|VERIFICATION\.txt|chocolatey(?:Install|BeforeModify|Uninstall)\.ps1)$',
        '^tools/captastic/(captastic(?:-desktop)?\.exe(?:\.ignore)?|captastic\.example\.toml|LICENSE-(?:MIT|APACHE)|README\.md)$',
        '^\[Content_Types\]\.xml$',
        '^package/services/metadata/core-properties/[0-9a-f]+\.psmdcp$'
    )
    foreach ($entry in $entries) {
        Assert-True (@($allowedPatterns | Where-Object { $entry -match $_ }).Count -gt 0) `
            "Chocolatey package contains unexpected entry $entry."
    }

    $verification = Get-ZipEntryText $packagePath 'tools/VERIFICATION.txt'
    Assert-True ($verification -notmatch '\{\{[^}]+\}\}') `
        'Chocolatey verification file contains unresolved placeholders.'
    Assert-True ($verification -match [regex]::Escape($sourceUrl)) `
        'Chocolatey verification file does not contain the source URL.'
    $archiveHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $chocolateyArchive).Hash.ToLowerInvariant()
    $cliHash = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $x64 'captastic.exe')).Hash.ToLowerInvariant()
    $desktopHash = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $x64 'captastic-desktop.exe')).Hash.ToLowerInvariant()
    foreach ($hash in @($archiveHash, $cliHash, $desktopHash)) {
        Assert-True ($verification -match $hash) "Chocolatey verification file does not contain hash $hash."
    }

    $nuspec = [xml](Get-ZipEntryText $packagePath 'captastic.nuspec')
    Assert-True ($nuspec.package.metadata.id -eq 'captastic') 'Chocolatey package id is incorrect.'
    Assert-True ($nuspec.package.metadata.iconUrl -like 'https://cdn.jsdelivr.net/*') `
        'Chocolatey icon URL is not served by the approved CDN.'
    Assert-True ($nuspec.package.metadata.iconUrl -notmatch '@(?:dev|main)/') `
        'Chocolatey icon URL is not immutable.'
    Assert-True ($nuspec.package.metadata.packageSourceUrl -eq `
        'https://github.com/stevenpickles/captastic/tree/v0.0.0-test-choco/packaging/chocolatey') `
        'Chocolatey package source URL is not version-specific.'
    Assert-True ($nuspec.package.metadata.licenseUrl -eq `
        'https://github.com/stevenpickles/captastic/blob/v0.0.0-test-choco/LICENSE-MIT') `
        'Chocolatey license URL is not version-specific.'
    foreach ($urlProperty in @(
        'projectUrl',
        'packageSourceUrl',
        'bugTrackerUrl',
        'licenseUrl',
        'projectSourceUrl',
        'docsUrl',
        'mailingListUrl',
        'iconUrl',
        'releaseNotes'
    )) {
        Assert-True ([string]$nuspec.package.metadata.$urlProperty -match '^https://') `
            "Chocolatey metadata URL $urlProperty is not HTTPS."
        Assert-True ([string]$nuspec.package.metadata.$urlProperty -notmatch 'raw\.githubusercontent\.com') `
            "Chocolatey metadata URL $urlProperty uses raw GitHub hosting."
    }

    Assert-ThrowsLike {
        & $chocolateyScript -Version '0.0.0-test-arm64' -Architecture arm64 `
            -ArchivePath $arm64Archive -OutputDirectory (Join-Path $testRoot 'arm-choco') `
            -SourceUrl 'https://example.invalid/arm64.zip'
    } '*currently supports only x86_64*'
    Assert-ThrowsLike {
        & $chocolateyScript -Version '0.0.0-test-choco' -Architecture x86_64 `
            -ArchivePath $chocolateyArchive -OutputDirectory (Join-Path $testRoot 'bad-publish-url') `
            -SourceUrl $sourceUrl -Publishable
    } '*source URL must be*'

    $staleOutput = Join-Path $testRoot 'stale-output'
    New-Item -ItemType Directory -Path $staleOutput -Force | Out-Null
    $stalePackage = Join-Path $staleOutput 'captastic.0.0.0-stale.nupkg'
    [System.IO.File]::WriteAllText($stalePackage, 'stale')
    Assert-ThrowsLike {
        & $chocolateyScript -Version '0.0.0-stale' -Architecture x86_64 `
            -ArchivePath (Join-Path $testRoot 'missing/captastic-0.0.0-stale-windows-x86_64.zip') `
            -OutputDirectory $staleOutput -SourceUrl 'https://example.invalid/missing.zip'
    } '*Portable release archive is missing*'
    Assert-True (-not (Test-Path -LiteralPath $stalePackage)) `
        'A stale Chocolatey package remained after a failed build attempt.'

    & (Join-Path $PSScriptRoot 'test-chocolatey-hooks.ps1')

    $metadata = cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
    $captasticPackage = @($metadata.packages | Where-Object {
        @($_.targets | Where-Object { $_.name -eq 'captastic' -and $_.kind -contains 'bin' }).Count -gt 0
    }) | Select-Object -First 1
    $orchestratedVersion = "$($captasticPackage.version)-ci.packaging"
    $orchestratedOutput = Join-Path $testRoot 'orchestrated-output'
    $result = & $orchestrationScript -Version $orchestratedVersion -Architecture x86_64 `
        -BinariesDirectory $x64 -OutputDirectory $orchestratedOutput `
        -SourceUrl 'https://example.invalid/orchestrated.zip' -SkipBuild
    Assert-True (Test-Path -LiteralPath $result.ManifestPath -PathType Leaf) `
        'Canonical packaging command did not create artifacts.json.'
    $manifest = Get-Content -LiteralPath $result.ManifestPath -Raw | ConvertFrom-Json
    Assert-True ($manifest.schemaVersion -eq 1) 'Artifact manifest schema version is incorrect.'
    Assert-True ($manifest.version -eq $orchestratedVersion) 'Artifact manifest version is incorrect.'
    Assert-True ($manifest.architecture -eq 'x86_64') 'Artifact manifest architecture is incorrect.'
    Assert-True ($manifest.publishable -eq $false) 'CI artifact manifest was incorrectly marked publishable.'
    Assert-True (@($manifest.artifacts).Count -eq 3) 'Artifact manifest does not list all expected artifacts.'
    foreach ($artifact in $manifest.artifacts) {
        $artifactPath = Join-Path $orchestratedOutput $artifact.file
        Assert-True (Test-Path -LiteralPath $artifactPath -PathType Leaf) `
            "Manifest artifact is missing: $($artifact.file)."
        Assert-True ((Get-FileHash -Algorithm SHA256 -LiteralPath $artifactPath).Hash.ToLowerInvariant() -eq $artifact.sha256) `
            "Manifest hash is incorrect for $($artifact.file)."
    }
    Assert-ThrowsLike {
        & $orchestrationScript -ReleaseTag 'v999.0.0' -Architecture x86_64 `
            -BinariesDirectory $x64 -OutputDirectory (Join-Path $testRoot 'tag-mismatch') -SkipBuild
    } '*must exactly match*'
    Assert-ThrowsLike {
        & $orchestrationScript -PrereleaseLabel 'ci.missing-url' -Architecture x86_64 `
            -BinariesDirectory $x64 -OutputDirectory (Join-Path $testRoot 'missing-source-url') -SkipBuild
    } '*require an explicit source archive URL*'

    Write-Host 'PowerShell packaging tests passed.'
} finally {
    Remove-Item -LiteralPath $testRoot -Recurse -Force -ErrorAction SilentlyContinue
}
