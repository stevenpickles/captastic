#Requires -Version 5.1

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repositoryRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))

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

function Test-EventExists([string]$Name) {
    $handle = $null
    if ([System.Threading.EventWaitHandle]::TryOpenExisting($Name, [ref]$handle)) {
        $handle.Dispose()
        return $true
    }
    return $false
}

# A stand-in daemon in its own process: it holds the control event, as a real daemon does, and
# either releases it when the event is signalled or ignores the request for longer than the hook
# waits. Its output says whether it was asked to stop.
function Start-FixtureDaemon([string]$Name, [bool]$StopsWhenAsked) {
    $job = Start-Job -ArgumentList $Name, $StopsWhenAsked -ScriptBlock {
        param([string]$Name, [bool]$StopsWhenAsked)
        $created = $false
        $controlEvent = New-Object System.Threading.EventWaitHandle(
            $false, [System.Threading.EventResetMode]::ManualReset, $Name, [ref]$created)
        try {
            if ($StopsWhenAsked) {
                [void]$controlEvent.WaitOne(15000)
            } else {
                Start-Sleep -Seconds 8
            }
            $controlEvent.WaitOne(0)
        } finally {
            $controlEvent.Dispose()
        }
    }
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while (-not (Test-EventExists $Name)) {
        if ([DateTime]::UtcNow -ge $deadline) {
            Remove-Job -Job $job -Force
            throw 'The fixture daemon did not create its control event.'
        }
        Start-Sleep -Milliseconds 50
    }
    return $job
}

$hookScript = Join-Path $repositoryRoot 'packaging\chocolatey\tools\chocolateyBeforeModify.ps1'
# Never the real name: these tests must not stop a daemon running in the session they run in.
$env:CAPTASTIC_HOOK_TEST_CONTROL_EVENT = "Local\CaptasticHookTest-$PID"
# The hook leaves its relaunch marker in TEMP; a directory of the test's own keeps a real one alone.
$previousTemp = $env:TEMP
$testTemp = Join-Path ([System.IO.Path]::GetTempPath()) "captastic-hook-tests-$PID"
New-Item -ItemType Directory -Path $testTemp -Force | Out-Null
$env:TEMP = $testTemp
$relaunchMarker = Join-Path $testTemp 'captastic-relaunch-after-upgrade'
$jobs = @()
try {
    # Nothing running: the hook has nothing to do, and nothing to start again afterwards.
    & $hookScript
    Assert-True (-not (Test-Path -LiteralPath $relaunchMarker)) `
        'Chocolatey before-modify hook asked for a relaunch when nothing was running.'

    $job = Start-FixtureDaemon $env:CAPTASTIC_HOOK_TEST_CONTROL_EVENT $true
    $jobs += $job
    & $hookScript
    Assert-True (-not (Test-EventExists $env:CAPTASTIC_HOOK_TEST_CONTROL_EVENT)) `
        'Chocolatey before-modify hook returned while the daemon still held its control event.'
    $signalled = @(Receive-Job -Job $job -Wait)[-1]
    Assert-True ($signalled -eq $true) 'Chocolatey before-modify hook did not signal the daemon.'
    Assert-True (Test-Path -LiteralPath $relaunchMarker -PathType Leaf) `
        'Chocolatey before-modify hook stopped a daemon without leaving the relaunch marker.'
    $stoppedAt = [DateTime]::Parse(
        (Get-Content -LiteralPath $relaunchMarker -Raw).Trim(),
        [System.Globalization.CultureInfo]::InvariantCulture,
        [System.Globalization.DateTimeStyles]::RoundtripKind
    )
    Assert-True (([DateTime]::UtcNow - $stoppedAt.ToUniversalTime()).TotalMinutes -lt 1) `
        'Chocolatey before-modify hook wrote a relaunch time that is not now.'
    Remove-Item -LiteralPath $relaunchMarker -Force

    $job = Start-FixtureDaemon $env:CAPTASTIC_HOOK_TEST_CONTROL_EVENT $false
    $jobs += $job
    Assert-ThrowsLike { & $hookScript } '*did not stop within five seconds*'
    Assert-True (-not (Test-Path -LiteralPath $relaunchMarker)) `
        'Chocolatey before-modify hook asked for a relaunch after failing to stop the daemon.'

    Write-Host "Chocolatey hook tests passed under PowerShell $($PSVersionTable.PSVersion)."
} finally {
    Remove-Item Env:CAPTASTIC_HOOK_TEST_CONTROL_EVENT -ErrorAction SilentlyContinue
    $env:TEMP = $previousTemp
    Remove-Item -LiteralPath $testTemp -Recurse -Force -ErrorAction SilentlyContinue
    foreach ($job in $jobs) {
        Remove-Job -Job $job -Force -ErrorAction SilentlyContinue
    }
}
