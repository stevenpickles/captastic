#Requires -Version 7.0

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Version,
    [ValidateSet('x86_64', 'arm64')]
    [string]$Architecture = 'x86_64',
    [string]$BinariesDirectory = (Join-Path $PSScriptRoot '..\target\dist'),
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '..\dist')
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$packageVersion = $Version.TrimStart('v')
if ($packageVersion -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$') {
    throw "Windows package version '$packageVersion' is not a valid SemVer version."
}
if (-not $PSBoundParameters.ContainsKey('BinariesDirectory') -and $Architecture -eq 'arm64') {
    $BinariesDirectory = Join-Path $repositoryRoot 'target\aarch64-pc-windows-msvc\dist'
}
$BinariesDirectory = [System.IO.Path]::GetFullPath($BinariesDirectory)
$OutputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)
$artifactVersion = $packageVersion -replace '[^A-Za-z0-9._-]', '-'
$packageName = "captastic-$artifactVersion-windows-$Architecture"
$packageDirectory = Join-Path $OutputDirectory $packageName
$archive = Join-Path $OutputDirectory "$packageName.zip"

$expectedMachine = if ($Architecture -eq 'arm64') { 0xAA64 } else { 0x8664 }

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
    $binaryPath = Join-Path $BinariesDirectory $binary
    if (-not (Test-Path -LiteralPath $binaryPath -PathType Leaf)) {
        throw "Distribution binary is missing: $binaryPath"
    }
    $machine = Get-PeMachine $binaryPath
    if ($machine -ne $expectedMachine) {
        throw ('Distribution binary {0} has PE machine 0x{1:X4}; expected 0x{2:X4} for {3}.' -f `
            $binaryPath, $machine, $expectedMachine, $Architecture)
    }
}

New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
Remove-Item -LiteralPath $packageDirectory -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $archive -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath "$archive.sha256" -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $packageDirectory -Force | Out-Null

try {
    foreach ($binary in @('captastic.exe', 'captastic-desktop.exe')) {
        Copy-Item -LiteralPath (Join-Path $BinariesDirectory $binary) -Destination $packageDirectory
    }
    foreach ($file in @(
        'captastic.example.toml',
        'README.md',
        'LICENSE-MIT',
        'LICENSE-APACHE'
    )) {
        $sourcePath = Join-Path $repositoryRoot $file
        if (-not (Test-Path -LiteralPath $sourcePath -PathType Leaf)) {
            throw "Required portable package file is missing: $sourcePath"
        }
        Copy-Item -LiteralPath $sourcePath -Destination $packageDirectory
    }
    foreach ($script in @('install.ps1', 'uninstall.ps1')) {
        $sourcePath = Join-Path $repositoryRoot "scripts\$script"
        if (-not (Test-Path -LiteralPath $sourcePath -PathType Leaf)) {
            throw "Required portable package script is missing: $sourcePath"
        }
        Copy-Item -LiteralPath $sourcePath -Destination $packageDirectory
    }

    Compress-Archive -Path $packageDirectory -DestinationPath $archive
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant()
    [System.IO.File]::WriteAllText(
        "$archive.sha256",
        "$hash  $packageName.zip`n",
        [System.Text.Encoding]::ASCII
    )
} finally {
    Remove-Item -LiteralPath $packageDirectory -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "Portable package: $archive"
Write-Host "SHA-256: $hash"
Write-Output $archive
