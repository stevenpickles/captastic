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
if (-not $PSBoundParameters.ContainsKey('BinariesDirectory') -and $Architecture -eq 'arm64') {
    $BinariesDirectory = Join-Path $repositoryRoot 'target\aarch64-pc-windows-msvc\dist'
}
$BinariesDirectory = [System.IO.Path]::GetFullPath($BinariesDirectory)
$OutputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)
$artifactVersion = $Version -replace '[^A-Za-z0-9._-]', '-'
$packageName = "captastic-$artifactVersion-windows-$Architecture"
$packageDirectory = Join-Path $OutputDirectory $packageName
$archive = Join-Path $OutputDirectory "$packageName.zip"

$expectedMachine = if ($Architecture -eq 'arm64') { 0xAA64 } else { 0x8664 }

function Get-PeMachine([string]$Path) {
    $stream = [System.IO.File]::OpenRead($Path)
    $reader = [System.IO.BinaryReader]::new($stream)
    try {
        if ($reader.ReadUInt16() -ne 0x5A4D) {
            throw "$Path is not a PE executable (missing MZ header)."
        }
        $stream.Position = 0x3C
        $peOffset = $reader.ReadUInt32()
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
New-Item -ItemType Directory -Path $packageDirectory -Force | Out-Null

foreach ($binary in @('captastic.exe', 'captastic-desktop.exe')) {
    Copy-Item -LiteralPath (Join-Path $BinariesDirectory $binary) -Destination $packageDirectory
}
foreach ($file in @(
    'captastic.example.toml',
    'README.md',
    'LICENSE-MIT',
    'LICENSE-APACHE'
)) {
    Copy-Item -LiteralPath (Join-Path $repositoryRoot $file) -Destination $packageDirectory
}
foreach ($script in @('install.ps1', 'uninstall.ps1')) {
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "scripts\$script") -Destination $packageDirectory
}

Remove-Item -LiteralPath $archive -Force -ErrorAction SilentlyContinue
Compress-Archive -Path $packageDirectory -DestinationPath $archive -Force
$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant()
[System.IO.File]::WriteAllText(
    "$archive.sha256",
    "$hash  $packageName.zip`n",
    [System.Text.Encoding]::ASCII
)

Write-Host "Portable package: $archive"
Write-Host "SHA-256: $hash"
Write-Output $archive
