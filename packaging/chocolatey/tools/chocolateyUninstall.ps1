$ErrorActionPreference = 'Stop'

$toolsDirectory = Split-Path -Parent $MyInvocation.MyCommand.Definition
$cli = Join-Path $toolsDirectory 'captastic\captastic.exe'

function Invoke-CaptasticCommand([string[]]$Arguments) {
    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $cli
    $startInfo.Arguments = $Arguments -join ' '
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $startInfo
    try {
        if (-not $process.Start()) {
            throw 'Captastic process did not start.'
        }
        $process.StandardOutput.ReadToEnd() | Out-Null
        $process.StandardError.ReadToEnd() | Out-Null
        $process.WaitForExit()
        return $process.ExitCode
    } finally {
        $process.Dispose()
    }
}

if (Test-Path -LiteralPath $cli -PathType Leaf) {
    # Captastic writes operational logs to stderr on successful commands;
    # suppress them because Chocolatey's host promotes native stderr to errors.
    $exitCode = Invoke-CaptasticCommand @('stop')
    if ($exitCode -ne 0) {
        Write-Warning "Captastic stop returned exit code $exitCode."
    }
    $exitCode = Invoke-CaptasticCommand @('startup', 'disable')
    if ($exitCode -ne 0) {
        Write-Warning "Disabling Captastic startup returned exit code $exitCode."
    }
}

Uninstall-BinFile -Name 'captastic'
Uninstall-BinFile -Name 'captastic-desktop'

$shortcutPath = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'Captastic.lnk'
if (Test-Path -LiteralPath $shortcutPath) {
    Remove-Item -LiteralPath $shortcutPath -Force
}

Write-Host 'Captastic was uninstalled.'
Write-Host 'Configuration and logs were retained under ~/.captastic.'
