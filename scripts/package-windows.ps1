[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Version,
    [string]$BinariesDirectory = (Join-Path $PSScriptRoot '..\target\dist'),
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '..\dist')
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$BinariesDirectory = [System.IO.Path]::GetFullPath($BinariesDirectory)
$OutputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)
$artifactVersion = $Version -replace '[^A-Za-z0-9._-]', '-'
$packageName = "captastic-$artifactVersion-windows-x86_64"
$packageDirectory = Join-Path $OutputDirectory $packageName
$archive = Join-Path $OutputDirectory "$packageName.zip"

foreach ($binary in @('captastic.exe', 'captastic-desktop.exe')) {
    $binaryPath = Join-Path $BinariesDirectory $binary
    if (-not (Test-Path -LiteralPath $binaryPath -PathType Leaf)) {
        throw "Distribution binary is missing: $binaryPath"
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
"$hash  $packageName.zip" | Set-Content -Encoding ascii "$archive.sha256"

Write-Host "Portable package: $archive"
Write-Host "SHA-256: $hash"
Write-Output $archive
