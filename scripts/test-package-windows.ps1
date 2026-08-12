#Requires -Version 7.0

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$packageScript = Join-Path $PSScriptRoot 'package-windows.ps1'
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

try {
    New-Item -ItemType Directory -Path $testRoot -Force | Out-Null

    $x64 = New-TestBinaries 'x64' 0x8664
    $x64Archive = & $packageScript -Version '0.0.0-test-x64' -Architecture x86_64 `
        -BinariesDirectory $x64 -OutputDirectory (Join-Path $testRoot 'x64-output') |
        Select-Object -Last 1
    if (-not (Test-Path -LiteralPath $x64Archive -PathType Leaf)) {
        throw 'x86_64 package test did not create an archive.'
    }

    $arm64 = New-TestBinaries 'arm64' 0xAA64
    $arm64Archive = & $packageScript -Version '0.0.0-test-arm64' -Architecture arm64 `
        -BinariesDirectory $arm64 -OutputDirectory (Join-Path $testRoot 'arm64-output') |
        Select-Object -Last 1
    if (-not (Test-Path -LiteralPath $arm64Archive -PathType Leaf)) {
        throw 'ARM64 package test did not create an archive.'
    }

    Assert-ThrowsLike {
        & $packageScript -Version '0.0.0-test-mismatch' -Architecture arm64 `
            -BinariesDirectory $x64 -OutputDirectory (Join-Path $testRoot 'mismatch-output')
    } '*has PE machine 0x8664; expected 0xAA64 for arm64*'

    $truncated = New-TestBinaries 'truncated' 0x8664
    [System.IO.File]::WriteAllBytes(
        (Join-Path $truncated 'captastic.exe'),
        [byte[]](0x4D, 0x5A)
    )
    Assert-ThrowsLike {
        & $packageScript -Version '0.0.0-test-truncated' -Architecture x86_64 `
            -BinariesDirectory $truncated -OutputDirectory (Join-Path $testRoot 'truncated-output')
    } '*truncated PE executable*DOS header is incomplete*'

    Write-Host 'PowerShell packaging tests passed.'
} finally {
    Remove-Item -LiteralPath $testRoot -Recurse -Force -ErrorAction SilentlyContinue
}
