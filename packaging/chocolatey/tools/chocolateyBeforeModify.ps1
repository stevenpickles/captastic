$ErrorActionPreference = 'Stop'

# Captastic is stopped through its session control event rather than through an executable. Every
# version's daemon holds this event while it runs and closes it as it exits, and every version's
# `stop` command did nothing but signal it. Going to the event directly is what lets this hook stop
# a 0.2 daemon and a 0.3 one alike, and 0.3 ships no command line to ask.
#
# CAPTASTIC_HOOK_TEST_CONTROL_EVENT exists for scripts/test-chocolatey-hooks.ps1 alone, so the hook
# tests drive a fixture instead of stopping a real daemon in the session they run in.
$controlEventName = 'Local\CaptasticDaemonControl-v1'
if ($env:CAPTASTIC_HOOK_TEST_CONTROL_EVENT) {
    $controlEventName = $env:CAPTASTIC_HOOK_TEST_CONTROL_EVENT
}

function Test-CaptasticRunning {
    $controlEvent = $null
    if (-not [System.Threading.EventWaitHandle]::TryOpenExisting($controlEventName, [ref]$controlEvent)) {
        return $false
    }
    $controlEvent.Dispose()
    return $true
}

$controlEvent = $null
if (-not [System.Threading.EventWaitHandle]::TryOpenExisting($controlEventName, [ref]$controlEvent)) {
    return
}
try {
    [void]$controlEvent.Set()
} finally {
    $controlEvent.Dispose()
}

$deadline = [DateTime]::UtcNow.AddSeconds(5)
do {
    Start-Sleep -Milliseconds 100
    $running = Test-CaptasticRunning
} while ($running -and [DateTime]::UtcNow -lt $deadline)

if ($running) {
    throw 'The running Captastic daemon did not stop within five seconds.'
}

# Left for chocolateyInstall.ps1, which runs next in an upgrade, so it can start Captastic again for
# the user it was just taken away from. An uninstall removes it instead. The time lets the install
# ignore one left by an upgrade that failed before it got that far.
[System.IO.File]::WriteAllText(
    (Join-Path $env:TEMP 'captastic-relaunch-after-upgrade'),
    [DateTime]::UtcNow.ToString('o')
)
