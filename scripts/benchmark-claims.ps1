<#
.SYNOPSIS
    Orchestrates the mechanical parts of the benchmark claim procedure.

.DESCRIPTION
    Builds the release binary, prints the preflight facts, runs one repeat set per cursor mode into
    benchmarks\runs\<date>\, and optionally compares each against a committed baseline.

    Nothing judged lives here. It does not decide that a preflight is acceptable, that a spread is
    small enough, or that a comparison is a regression - those are the operator's, and a script that
    made them would turn the acceptance criteria into something nobody reads. The procedure and the
    criteria are in benchmarks\README.md; this only saves the typing.

.EXAMPLE
    .\scripts\benchmark-claims.ps1 -BaselineDir benchmarks\baselines\workshop\v0.2.0
#>
[CmdletBinding()]
param(
    [string] $Backend = 'dxgi',
    [int]    $Iterations = 200,
    [int]    $Warmup = 20,
    [int]    $Repeat = 3,
    [string] $OutputRoot = (Join-Path 'benchmarks' (Join-Path 'runs' (Get-Date -Format 'yyyy-MM-dd'))),
    [string] $BaselineDir
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Push-Location $repo
try {
    Write-Host '== build =='
    cargo build --release -p captastic-app
    $exe = Join-Path $repo 'target\release\captastic.exe'

    Write-Host "`n== preflight (benchmarks\README.md step 2 says what each must be) =="
    $doctor = & $exe doctor --json | ConvertFrom-Json
    $environment = $doctor.environment
    $driving = $environment.adapters | Select-Object -First 1
    [pscustomobject]@{
        session          = $environment.session
        power_source     = $environment.power_source
        software_adapter = $driving.software
        adapter          = $driving.description
        build_dirty      = $environment.build.dirty
        build            = $environment.build.version
    } | Format-List

    Write-Host '== keep something repainting on the primary display for every run =='
    Write-Host 'Press Enter to start, or Ctrl+C to stop here.'
    [void](Read-Host)

    foreach ($cursor in @('exclude', 'include')) {
        $cell = "latest-$cursor"
        $directory = Join-Path $OutputRoot $cell
        Write-Host "`n== run: $cell -> $directory =="
        & $exe benchmark `
            --backend $Backend --mode latest --cursor $cursor --cpu-frame true `
            --repeat $Repeat --iterations $Iterations --warmup $Warmup `
            --budgets (Join-Path 'benchmarks' 'budgets.toml') `
            --raw-events 'events.jsonl' --output-dir $directory
        if ($LASTEXITCODE -ne 0) {
            # Reported, not swallowed and not acted on: an incompatible set or a breached budget is
            # exactly what the operator has to see before deciding anything about the other cells.
            Write-Warning "$cell exited $LASTEXITCODE; see the output above."
        }

        if ($BaselineDir) {
            $baseline = Join-Path (Join-Path $BaselineDir $cell) 'repeated.json'
            if (Test-Path $baseline) {
                Write-Host "`n== compare: $cell against $baseline =="
                & $exe benchmark compare $baseline (Join-Path $directory 'repeated.json')
            }
            else {
                Write-Warning "no committed baseline at $baseline; nothing to compare $cell against."
            }
        }
    }

    Write-Host "`nArtifacts are under $OutputRoot. Accept or discard each set by the criteria in"
    Write-Host 'benchmarks\README.md step 4 before anything from them is quoted.'
}
finally {
    Pop-Location
}
