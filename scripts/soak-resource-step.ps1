<#
.SYNOPSIS
    Hunts the unexplained GDI/USER/handle step from issue #53.

.DESCRIPTION
    The step observed on 2026-08-17 was +22 GDI, +21 USER and +65 handles at roughly three
    minutes, then dead flat for six. Flat afterwards is the signature of one-time lazy
    initialisation rather than a leak, so running the same soak for longer would mostly re-observe
    it. This run is built to discriminate instead, by changing one thing at a time.

    Two legs, in order:

      idle   the daemon runs and captures nothing. If the step appears here, it is time-based or
             environmental and has nothing to do with capture work at all - the single most
             valuable thing to rule out, and the cheapest.
      busy   the daemon self-triggers 4K DXGI captures with the clipboard and file output OFF.
             The original run had both on, so a step there could have come from clipboard format
             synthesis or from the file writer; with both removed, anything that survives is the
             capture path itself.

    Sampling is every five seconds rather than ten, so a step can be tied to a log line rather
    than to a ten-second window, and the daemon's own log is kept for that correlation.

    The user's own daemon is stopped for the duration (the control event is single-instance) and
    restarted at the end, including on failure.

.EXAMPLE
    .\soak-resource-step.ps1 -OutDir C:\temp\step -IdleMinutes 8 -BusyMinutes 40
#>
[CmdletBinding()]
param(
    [string] $Exe = 'C:\Users\Steven\work\captastic\target\release\captastic.exe',
    [string] $UserExe = 'C:\ProgramData\chocolatey\lib\captastic\tools\captastic\captastic.exe',
    [Parameter(Mandatory)] [string] $OutDir,
    [int] $IdleMinutes = 8,
    [int] $BusyMinutes = 40,
    [int] $TriggerIntervalMs = 500,
    [int] $SampleSeconds = 5
)

$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $OutDir | Out-Null
$log = Join-Path $OutDir 'run.log'
$sampler = Join-Path $PSScriptRoot 'sample-process-resources.ps1'

# Windows' idle timer watches user input, not DXGI activity, so an unattended soak can have the
# display sleep underneath it. That is not a neutral event for this measurement: the outputs may
# detach, captures fail into the recovery path, and the recovery churn lands in the very counters
# being watched. Held for the length of the run and released with it - nothing about the user's
# power settings is changed, so an interrupted run cannot leave their display awake forever.
#
# Display power is itself a suspect for the #53 step and deserves its own controlled run. It is
# suppressed here so that the baseline says what steady capture does, with nothing else moving.
Add-Type -Namespace Captastic -Name Power -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError = true)]
public static extern uint SetThreadExecutionState(uint esFlags);
'@
# Decimal, not 0x80000000: PowerShell parses that hex literal as a negative Int32 before any cast
# can apply, so `[uint32] 0x80000000` fails outright. 2147483648 parses as Int64 and converts.
[uint32] $ES_CONTINUOUS = 2147483648
[uint32] $ES_SYSTEM_REQUIRED = 0x00000001
[uint32] $ES_DISPLAY_REQUIRED = 0x00000002

function Write-Line([string] $text) {
    $stamp = (Get-Date).ToString('HH:mm:ss')
    "$stamp  $text" | Tee-Object -FilePath $log -Append | Out-Null
}

# Runs one leg: start a daemon, sample it for the duration, stop it, and keep everything.
function Invoke-Leg([string] $Name, [int] $Minutes, [string[]] $ExtraArgs) {
    $daemonOut = Join-Path $OutDir "$Name-daemon.log"
    $csv = Join-Path $OutDir "$Name-resources.csv"
    $arguments = @('daemon', '--backend', 'dxgi', '--clipboard', 'false', '--selection', 'false') + $ExtraArgs
    Write-Line "$Name leg: starting daemon ($($arguments -join ' '))"
    $daemon = Start-Process -FilePath $Exe -ArgumentList $arguments -RedirectStandardOutput "$daemonOut.out" `
        -RedirectStandardError $daemonOut -PassThru -WindowStyle Hidden
    Start-Sleep -Seconds 3
    if ($daemon.HasExited) {
        Write-Line "$Name leg: daemon exited immediately with code $($daemon.ExitCode); aborting leg"
        return
    }
    Write-Line "$Name leg: daemon pid $($daemon.Id); sampling every ${SampleSeconds}s for ${Minutes}m"
    & $sampler -ProcessId $daemon.Id -IntervalSeconds $SampleSeconds -OutputPath $csv -MaxMinutes $Minutes
    Write-Line "$Name leg: sampling complete; stopping daemon"
    & $Exe stop 2>&1 | ForEach-Object { Write-Line "  $_" }
    Start-Sleep -Seconds 3
    if (-not $daemon.HasExited) {
        Stop-Process -Id $daemon.Id -Force
        Write-Line "$Name leg: daemon force-stopped"
    }
}

$userWasRunning = [bool] (Get-Process captastic -ErrorAction SilentlyContinue)
try {
    [uint32] $keepAwake = $ES_CONTINUOUS -bor $ES_SYSTEM_REQUIRED -bor $ES_DISPLAY_REQUIRED
    $held = [Captastic.Power]::SetThreadExecutionState($keepAwake)
    if ($held -eq 0) {
        Write-Line 'WARNING: could not hold the display awake; a sleep during the run will confound it'
    } else {
        Write-Line 'holding the display awake for the duration of the run'
    }
    if ($userWasRunning) {
        Write-Line "stopping the user's daemon for the duration"
        & $UserExe stop 2>&1 | Out-Null
        Start-Sleep -Seconds 3
    }

    # Control: no capture work at all. A step here indicts the environment, not Captastic.
    Invoke-Leg -Name 'idle' -Minutes $IdleMinutes -ExtraArgs @()

    # Treatment: capture work, with the two destinations from the original run removed.
    Invoke-Leg -Name 'busy' -Minutes $BusyMinutes -ExtraArgs @(
        '--self-trigger', '--self-trigger-interval-ms', "$TriggerIntervalMs"
    )
}
finally {
    # Back to whatever the user's power plan says, immediately.
    [void] [Captastic.Power]::SetThreadExecutionState($ES_CONTINUOUS)
    if ($userWasRunning) {
        Write-Line "restarting the user's daemon"
        Start-Process -FilePath $UserExe -WindowStyle Hidden
        Start-Sleep -Seconds 4
        $back = Get-Process captastic -ErrorAction SilentlyContinue
        Write-Line ("user's daemon: " + $(if ($back) { "running (pid $($back.Id))" } else { 'NOT RUNNING - restart by hand' }))
    }
    Write-Line 'done'
}
