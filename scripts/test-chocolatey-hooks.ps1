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
$jobs = @()
try {
    # Nothing running: the hook has nothing to do.
    & $hookScript

    $job = Start-FixtureDaemon $env:CAPTASTIC_HOOK_TEST_CONTROL_EVENT $true
    $jobs += $job
    & $hookScript
    Assert-True (-not (Test-EventExists $env:CAPTASTIC_HOOK_TEST_CONTROL_EVENT)) `
        'Chocolatey before-modify hook returned while the daemon still held its control event.'
    $signalled = @(Receive-Job -Job $job -Wait)[-1]
    Assert-True ($signalled -eq $true) 'Chocolatey before-modify hook did not signal the daemon.'

    $job = Start-FixtureDaemon $env:CAPTASTIC_HOOK_TEST_CONTROL_EVENT $false
    $jobs += $job
    Assert-ThrowsLike { & $hookScript } '*did not stop within five seconds*'

    Write-Host "Chocolatey hook tests passed under PowerShell $($PSVersionTable.PSVersion)."
} finally {
    Remove-Item Env:CAPTASTIC_HOOK_TEST_CONTROL_EVENT -ErrorAction SilentlyContinue
    foreach ($job in $jobs) {
        Remove-Job -Job $job -Force -ErrorAction SilentlyContinue
    }
}
