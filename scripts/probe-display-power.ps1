<#
.SYNOPSIS
    Tests whether powering the display off and on causes the GDI/USER step from issue #53.

.DESCRIPTION
    Three soak runs - 12,420 captures over 79 minutes - held GDI at exactly 10 in every sample,
    exonerating the capture path, both destinations and the BufferExhausted refusal path. The one
    variable that differed in all of them is display power: uncontrolled during the original run,
    deliberately suppressed during the reproductions. It also fits the shape of the original
    observation exactly - a single event that allocates a batch of objects and then holds flat
    regardless of how much work follows.

    So this powers the panel down and back up while sampling the daemon's counters every two
    seconds, with captures running throughout. Three questions in one run:

      1. does display power cause the #53 step?
      2. does a powered-down display detach from enumeration? (#60, and the cause of #51's log)
      3. does the monitor's persistent identity survive the power cycle? (#60)

    The display is woken by synthesising a zero-delta mouse move as well as by asking for it, and
    the wake is repeated on the way out, so a failed wake cannot leave a black screen. Any keypress
    or mouse movement restores it regardless.
#>
[CmdletBinding()]
param(
    [string] $Exe = 'C:\Users\Steven\work\captastic\target\release\captastic.exe',
    [string] $UserExe = 'C:\ProgramData\chocolatey\lib\captastic\tools\captastic\captastic.exe',
    [Parameter(Mandatory)] [string] $OutDir,
    [int] $BaselineSeconds = 30,
    [int] $OffSeconds = 25,
    [int] $AfterSeconds = 45,
    [int] $SampleSeconds = 2
)

$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $OutDir | Out-Null
$log = Join-Path $OutDir 'run.log'
$csv = Join-Path $OutDir 'samples.csv'
$daemonLog = Join-Path $OutDir 'daemon.log'

function Write-Line([string] $text) {
    $stamp = (Get-Date).ToString('HH:mm:ss')
    "$stamp  $text" | Tee-Object -FilePath $log -Append | Out-Null
}

$nativeSource = @"
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern System.IntPtr SendMessageTimeout(System.IntPtr hWnd, uint msg, System.IntPtr wParam,
    System.IntPtr lParam, uint flags, uint timeout, out System.IntPtr result);
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, System.IntPtr extra);
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern uint GetGuiResources(System.IntPtr process, uint flags);
"@
Add-Type -Namespace Probe -Name Native -MemberDefinition $nativeSource

$HWND_BROADCAST = [System.IntPtr] 0xffff
$WM_SYSCOMMAND = 0x0112
$SC_MONITORPOWER = [System.IntPtr] 0xF170
$SMTO_ABORTIFHUNG = 0x0002
$MOUSEEVENTF_MOVE = 0x0001

function Set-MonitorPower([int] $State) {
    # 2 powers the panel down, -1 brings it back. Broadcast with a timeout rather than a plain
    # SendMessage, so one unresponsive top-level window cannot wedge this script.
    $result = [System.IntPtr]::Zero
    [void] [Probe.Native]::SendMessageTimeout($HWND_BROADCAST, $WM_SYSCOMMAND, $SC_MONITORPOWER,
        [System.IntPtr] $State, $SMTO_ABORTIFHUNG, 2000, [ref] $result)
}

# A zero-delta move is real input as far as Windows is concerned, so it both wakes the panel and
# resets the idle timer. Asking with SC_MONITORPOWER -1 alone is not always honoured.
function Invoke-DisplayWake {
    [Probe.Native]::mouse_event($MOUSEEVENTF_MOVE, 0, 0, 0, [System.IntPtr]::Zero)
    Set-MonitorPower -State -1
    [Probe.Native]::mouse_event($MOUSEEVENTF_MOVE, 0, 0, 0, [System.IntPtr]::Zero)
}

# What Captastic makes of the display right now: how many it can see, and the identity it derives.
function Get-DisplayIdentity {
    try {
        $json = & $Exe displays --backend dxgi --json 2>$null | Out-String
        $parsed = $json | ConvertFrom-Json -ErrorAction Stop
        $displays = @($parsed)
        if ($displays.Count -eq 0) { return @{ Count = 0; Id = 'none' } }
        return @{ Count = $displays.Count; Id = $displays[0].id }
    } catch {
        return @{ Count = -1; Id = 'query-failed' }
    }
}

$userWasRunning = [bool] (Get-Process captastic -ErrorAction SilentlyContinue)
$daemon = $null
try {
    if ($userWasRunning) {
        Write-Line 'stopping the user daemon for the duration'
        & $UserExe stop 2>&1 | Out-Null
        Start-Sleep -Seconds 3
    }

    # Captures run throughout, so the same run also says whether capture recovers after a wake -
    # the sleep/wake half of Milestone 5's lifecycle-recovery bullet.
    $arguments = @('daemon', '--backend', 'dxgi', '--clipboard', 'false', '--selection', 'false',
        '--self-trigger', '--self-trigger-interval-ms', '2000')
    $daemon = Start-Process -FilePath $Exe -ArgumentList $arguments -RedirectStandardOutput "$daemonLog.out" -RedirectStandardError $daemonLog -PassThru -WindowStyle Hidden
    Start-Sleep -Seconds 3
    if ($daemon.HasExited) {
        Write-Line "daemon exited immediately with code $($daemon.ExitCode); aborting"
        return
    }
    Write-Line "daemon pid $($daemon.Id); baseline ${BaselineSeconds}s, display off ${OffSeconds}s, then ${AfterSeconds}s"

    'Timestamp,ElapsedSeconds,Phase,Handles,GdiObjects,UserObjects,PrivateMB,DisplayCount,DisplayId' |
        Set-Content -Path $csv -Encoding UTF8

    $started = Get-Date
    $total = $BaselineSeconds + $OffSeconds + $AfterSeconds
    $poweredOff = $false
    $wokeUp = $false
    $iteration = 0
    while (((Get-Date) - $started).TotalSeconds -lt $total) {
        $elapsed = [int] ((Get-Date) - $started).TotalSeconds

        if (-not $poweredOff -and $elapsed -ge $BaselineSeconds) {
            Write-Line 'powering the display DOWN'
            Set-MonitorPower -State 2
            $poweredOff = $true
        }
        if ($poweredOff -and -not $wokeUp -and $elapsed -ge ($BaselineSeconds + $OffSeconds)) {
            Write-Line 'waking the display'
            Invoke-DisplayWake
            $wokeUp = $true
        }

        $phase = if (-not $poweredOff) { 'baseline' } elseif (-not $wokeUp) { 'display-off' } else { 'after-wake' }
        $process = Get-Process -Id $daemon.Id -ErrorAction SilentlyContinue
        if ($null -eq $process) { Write-Line 'daemon vanished'; break }
        $gdi = [Probe.Native]::GetGuiResources($process.Handle, 0)
        $user = [Probe.Native]::GetGuiResources($process.Handle, 1)
        $privateMb = [math]::Round($process.PrivateMemorySize64 / 1MB, 2)

        # Enumeration is queried sparsely: it spawns a process, and the question it answers changes
        # on the scale of the phase rather than the sample.
        $identity = @{ Count = ''; Id = '' }
        if ($iteration % 3 -eq 0) { $identity = Get-DisplayIdentity }

        '{0},{1},{2},{3},{4},{5},{6},{7},{8}' -f (Get-Date).ToString('HH:mm:ss'), $elapsed, $phase,
            $process.HandleCount, $gdi, $user, $privateMb, $identity.Count, $identity.Id |
            Add-Content -Path $csv -Encoding UTF8

        $iteration++
        Start-Sleep -Seconds $SampleSeconds
    }
}
finally {
    # Belt and braces: whatever happened above, the panel must be on when this exits.
    Invoke-DisplayWake
    Start-Sleep -Milliseconds 500
    Invoke-DisplayWake
    Write-Line 'display wake requested on the way out'
    if ($daemon -and -not $daemon.HasExited) {
        & $Exe stop 2>&1 | Out-Null
        Start-Sleep -Seconds 3
        if (-not $daemon.HasExited) { Stop-Process -Id $daemon.Id -Force }
    }
    if ($userWasRunning) {
        Start-Process -FilePath $UserExe -WindowStyle Hidden
        Start-Sleep -Seconds 4
        $back = Get-Process captastic -ErrorAction SilentlyContinue
        Write-Line ('user daemon: ' + $(if ($back) { "running (pid $($back.Id))" } else { 'NOT RUNNING' }))
    }
    Write-Line 'done'
}
