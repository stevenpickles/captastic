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
        $stdout = $process.StandardOutput.ReadToEnd()
        $process.StandardError.ReadToEnd() | Out-Null
        $process.WaitForExit()
        return [PSCustomObject]@{
            ExitCode = $process.ExitCode
            Stdout = $stdout
        }
    } finally {
        $process.Dispose()
    }
}

if (-not (Test-Path -LiteralPath $cli -PathType Leaf)) {
    return
}

$status = $null
try {
    # Captastic writes operational logs to stderr even when a command succeeds.
    # Keep native streams separate because Chocolatey's PowerShell host promotes
    # native stderr to a terminating error when ErrorActionPreference is Stop.
    $statusResult = Invoke-CaptasticCommand @('status', '--json')
    if ($statusResult.ExitCode -ne 0) {
        throw "Captastic status failed with exit code $($statusResult.ExitCode)."
    }
    $status = ($statusResult.Stdout | ConvertFrom-Json).status
} catch {
    throw "Captastic status could not be read before package modification: $($_.Exception.Message)"
}

if ($status -eq 'not_running') {
    return
}
if ($status -ne 'running') {
    throw "Captastic returned unexpected status '$status'; refusing package modification."
}

$stopResult = Invoke-CaptasticCommand @('stop')
if ($stopResult.ExitCode -ne 0) {
    throw "Captastic stop failed with exit code $($stopResult.ExitCode)."
}

$deadline = [DateTime]::UtcNow.AddSeconds(5)
do {
    Start-Sleep -Milliseconds 100
    $statusResult = Invoke-CaptasticCommand @('status', '--json')
    if ($statusResult.ExitCode -ne 0) {
        throw "Captastic status failed with exit code $($statusResult.ExitCode) while waiting for shutdown."
    }
    $status = ($statusResult.Stdout | ConvertFrom-Json).status
} while ($status -eq 'running' -and [DateTime]::UtcNow -lt $deadline)

if ($status -eq 'running') {
    throw 'The running Captastic daemon did not stop within five seconds.'
}
if ($status -ne 'not_running') {
    throw "Captastic returned unexpected status '$status' after stop; refusing package modification."
}
