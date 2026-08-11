$ErrorActionPreference = 'Stop'

$toolsDirectory = Split-Path -Parent $MyInvocation.MyCommand.Definition
$cli = Join-Path $toolsDirectory 'captastic\captastic.exe'

if (-not (Test-Path -LiteralPath $cli -PathType Leaf)) {
    return
}

$status = $null
try {
    $status = (& $cli status --json | ConvertFrom-Json).status
} catch {
    Write-Warning "Captastic status could not be read before package modification: $($_.Exception.Message)"
}

if ($status -ne 'running') {
    return
}

& $cli stop | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "Captastic stop failed with exit code $LASTEXITCODE."
}

$deadline = [DateTime]::UtcNow.AddSeconds(5)
do {
    Start-Sleep -Milliseconds 100
    $status = (& $cli status --json | ConvertFrom-Json).status
} while ($status -eq 'running' -and [DateTime]::UtcNow -lt $deadline)

if ($status -eq 'running') {
    throw 'The running Captastic daemon did not stop within five seconds.'
}
