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

function Invoke-GitText([string[]]$Arguments) {
    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'SilentlyContinue'
    try {
        $output = & git -C $repositoryRoot @Arguments 2>$null
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    if ($exitCode -ne 0) {
        return $null
    }
    return ([string]($output -join "`n")).Trim()
}

function ConvertTo-OptionalUInt64([string]$Value, [string]$Name) {
    if ([string]::IsNullOrWhiteSpace($Value)) {
        return $null
    }
    [UInt64]$parsed = 0
    if (-not [UInt64]::TryParse($Value, [ref]$parsed)) {
        throw "$Name must be an unsigned integer."
    }
    return [UInt64]$parsed
}

function Get-BinaryBuildMetadata([string]$Directory) {
    $items = foreach ($binary in @('captastic.exe', 'captastic-desktop.exe')) {
        $path = Join-Path $Directory $binary
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "Distribution binary is missing: $path"
        }
        $versionInfo = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($path)
        if ([string]::IsNullOrWhiteSpace($versionInfo.ProductVersion)) {
            throw "Distribution binary does not contain Captastic build metadata: $path"
        }
        $commit = if ($versionInfo.Comments -match '^Git commit: (?<commit>[0-9a-fA-F]{40})$') {
            $Matches.commit.ToLowerInvariant()
        } else {
            $null
        }
        [PSCustomObject]@{
            File = $binary
            Version = [string]$versionInfo.ProductVersion
            Commit = $commit
        }
    }
    $versions = @($items.Version | Select-Object -Unique)
    if ($versions.Count -ne 1) {
        throw "Distribution binaries have different embedded build versions: $($versions -join ', ')."
    }
    $commits = @($items.Commit | Where-Object { $null -ne $_ } | Select-Object -Unique)
    if ($commits.Count -gt 1) {
        throw "Distribution binaries have different embedded Git commits: $($commits -join ', ')."
    }
    return [PSCustomObject]@{
        Version = $versions[0]
        Commit = if ($commits.Count -eq 1) { $commits[0] } else { $null }
    }
}

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
$expectedTag = "v$workspaceVersion"
$gitCommit = if (-not [string]::IsNullOrWhiteSpace($env:CAPTASTIC_GIT_COMMIT)) {
    $env:CAPTASTIC_GIT_COMMIT.Trim().ToLowerInvariant()
} elseif (-not [string]::IsNullOrWhiteSpace($env:GITHUB_SHA)) {
    $env:GITHUB_SHA.Trim().ToLowerInvariant()
} else {
    Invoke-GitText @('rev-parse', 'HEAD')
}
$gitDirty = if (-not [string]::IsNullOrWhiteSpace($env:CAPTASTIC_GIT_DIRTY)) {
    if ($env:CAPTASTIC_GIT_DIRTY -notmatch '^(?:true|false|1|0)$') {
        throw 'CAPTASTIC_GIT_DIRTY must be true or false.'
    }
    $env:CAPTASTIC_GIT_DIRTY -in @('true', '1')
} else {
    -not [string]::IsNullOrWhiteSpace((Invoke-GitText @('status', '--porcelain', '--untracked-files=normal')))
}
$tagsAtHead = @((Invoke-GitText @('tag', '--points-at', 'HEAD')) -split "`r?`n" |
    Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
$sourceTag = if (-not [string]::IsNullOrWhiteSpace($ReleaseTag)) {
    $ReleaseTag
} elseif ($tagsAtHead -contains $expectedTag) {
    $expectedTag
} else {
    $null
}
$latestTag = Invoke-GitText @('describe', '--tags', '--match', 'v[0-9]*', '--abbrev=0')
$revisionRange = if ([string]::IsNullOrWhiteSpace($latestTag)) { 'HEAD' } else { "$latestTag..HEAD" }
$revisionCountText = Invoke-GitText @('rev-list', '--count', $revisionRange)
$revisionCount = ConvertTo-OptionalUInt64 $revisionCountText 'Git revision count'
$isCi = -not [string]::IsNullOrWhiteSpace($env:GITHUB_ACTIONS)
$buildChannel = if ($null -ne $sourceTag) { 'release' } elseif ($isCi) { 'ci' } else { 'development' }
$ciRunId = if ([string]::IsNullOrWhiteSpace($env:GITHUB_RUN_ID)) { $null } else { $env:GITHUB_RUN_ID }
$ciRunNumber = ConvertTo-OptionalUInt64 $env:GITHUB_RUN_NUMBER 'GITHUB_RUN_NUMBER'
$ciRunAttempt = ConvertTo-OptionalUInt64 $env:GITHUB_RUN_ATTEMPT 'GITHUB_RUN_ATTEMPT'
$ciRunUrl = if ($isCi -and
    -not [string]::IsNullOrWhiteSpace($env:GITHUB_SERVER_URL) -and
    -not [string]::IsNullOrWhiteSpace($env:GITHUB_REPOSITORY) -and
    $null -ne $ciRunId) {
    "$($env:GITHUB_SERVER_URL)/$($env:GITHUB_REPOSITORY)/actions/runs/$ciRunId"
} else {
    $null
}
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
    if ($ReleaseTag -ne $expectedTag -or $packageVersion -ne $workspaceVersion) {
        throw "Release tag $ReleaseTag must exactly match $expectedTag and package version $workspaceVersion."
    }
    if ($tagsAtHead -notcontains $ReleaseTag) {
        throw "Release tag $ReleaseTag does not point at HEAD."
    }
    if ($gitDirty) {
        throw 'Release package builds require a clean worktree.'
    }
    if ([string]::IsNullOrWhiteSpace($gitCommit)) {
        throw 'Release package builds require a Git commit.'
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

$binaryBuild = Get-BinaryBuildMetadata $BinariesDirectory
$embeddedCore = ($binaryBuild.Version -split '-', 2)[0]
if ($embeddedCore -ne $workspaceVersion) {
    throw "Embedded build version $($binaryBuild.Version) does not use workspace version $workspaceVersion as its release core."
}
if ($SkipBuild -and
    -not [string]::IsNullOrWhiteSpace($gitCommit) -and
    -not [string]::IsNullOrWhiteSpace($binaryBuild.Commit) -and
    $binaryBuild.Commit -ne $gitCommit) {
    throw "Distribution binaries were built from $($binaryBuild.Commit), but the current source is $gitCommit."
}
if (-not [string]::IsNullOrWhiteSpace($ReleaseTag) -and
    $binaryBuild.Version -ne $workspaceVersion) {
    throw "Release binaries must embed version $workspaceVersion; found $($binaryBuild.Version)."
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
    schemaVersion = 2
    version = $packageVersion
    workspaceVersion = $workspaceVersion
    buildVersion = $binaryBuild.Version
    build = [ordered]@{
        version = $binaryBuild.Version
        channel = $buildChannel
        gitCommit = if ($null -ne $binaryBuild.Commit) { $binaryBuild.Commit } else { $gitCommit }
        revisionCount = $revisionCount
        sourceTag = $sourceTag
        dirty = $gitDirty
        ciRunId = $ciRunId
        ciRunNumber = $ciRunNumber
        ciRunAttempt = $ciRunAttempt
        ciRunUrl = $ciRunUrl
    }
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
