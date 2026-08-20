<#
.SYNOPSIS
    Measures whether a daemon holding a live duplication recovers on its own across
    lock -> displays asleep -> unlock.

.DESCRIPTION
    Issue #51 ended with a locked session's duplication failures being *explained*: the harness in
    session.rs (a_locked_session_explains_every_duplication_failure) proved that every refusal
    arrives with the lock attached to it. That is diagnosis. It says nothing about whether the
    daemon comes back, and the daemon's behaviour while locked changed when the lock query landed -
    a locked session used to read as interactive and fall through to the display poll, and now
    takes the session poll instead. This measures the product end to end, and nothing in it is
    faked: a real daemon captures once a second, the workstation really locks, the displays really
    power down, and the daemon's own log is the evidence.

    Expected, in order:
      1. baseline: the daemon self-triggers a capture every --IntervalMs and they succeed
      2. the workstation locks; captures keep succeeding while the lock screen is lit
      3. the displays power down; the in-flight duplication dies and the rebuild is refused
      4. the refusal is classified DesktopUnavailable, so the daemon waits on the session probe
         rather than rebuilding DXGI twice a second - and never walks the adapter list
      5. the operator returns and unlocks; the daemon rebuilds and captures resume, untouched

    A run that reaches (3) and never reaches (5) is the finding this exercise exists to look for.

    The measurement lives in the daemon's log rather than in this script's sampling loop, which is
    deliberate: a locked session suspends applications, and a broadcast that waits on each of them
    in turn once ate four minutes of an earlier harness and skipped its loop entirely. Here that
    broadcast runs in a background job, and a sampling loop that stalls costs cross-checks rather
    than the measurement.

.NOTES
    WITHOUT -Confirmed this is a dry run: the daemon starts, captures, and is read back, and the
    workstation is never locked. That is the smoke test for the plumbing, and it is the default so
    that no accidental invocation can take the desktop away.

    WITH -Confirmed it LOCKS THE WORKSTATION and POWERS THE DISPLAYS DOWN. Nothing here can undo
    that; the operator unlocks by signing back in, whenever they return. Any input before then
    wakes the displays and ends the condition being measured, so the machine has to be left alone.

    cargo build -p captastic-app
    powershell -File scripts\measure-lock-unlock-recovery.ps1 -OutDir outputs\lock-unlock -Confirmed
#>
[CmdletBinding()]
param(
    # The repository this script lives in, rather than one machine's home directory.
    [string] $Repo = (Split-Path $PSScriptRoot -Parent),
    [Parameter(Mandatory = $true)]
    [string] $OutDir,
    # The go signal. Absent, this locks nothing and runs as a smoke test of everything else.
    [switch] $Confirmed,
    # One capture a second: fast enough that the three seconds between the displays powering down
    # and duplication being refused cannot slip between two captures, slow enough that a run
    # outlasting a coffee break is a few hundred log lines rather than a few thousand.
    [int] $IntervalMs = 1000,
    # Healthy captures before the lock, so the run has a baseline to have lost.
    [int] $BaselineSeconds = 30,
    # What the operator is asked to stay away for. Not enforced - they unlock when they unlock -
    # but the verdict says so when a run came back too early to have measured anything.
    [int] $MinimumLockSeconds = 180,
    # How long to keep waiting for the operator. A run that gives up restores everything and
    # reports that it measured nothing, which is better than a daemon left stopped.
    [int] $UnlockWaitMinutes = 20,
    # Captures after the unlock, so "it recovered" is a run of successes rather than one.
    [int] $AfterUnlockSeconds = 60,
    # The lock-flag timeline this script keeps for itself, independent of the daemon under test.
    [int] $SampleMs = 1000,
    [string] $Exe = (Join-Path (Split-Path $PSScriptRoot -Parent) 'target\debug\captastic.exe'),
    [string] $UserExe = 'C:\ProgramData\chocolatey\lib\captastic\tools\captastic\captastic.exe'
)

$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $OutDir | Out-Null
$runLog = Join-Path $OutDir 'run.log'
$csv = Join-Path $OutDir 'lock-samples.csv'
$log = Join-Path $OutDir 'daemon.log'
$out = Join-Path $OutDir 'daemon.out'
$err = Join-Path $OutDir 'daemon.err'

function Write-Line([string] $text) {
    $stamp = [DateTime]::UtcNow.ToString('HH:mm:ss')
    "$stamp  $text" | Tee-Object -FilePath $runLog -Append
}

# ---------------------------------------------------------------------------------------------
# The lock flag, asked of Windows directly rather than of the crate under test.
#
# WTSSessionInfoEx is the only signal that answers in every phase of a lock: Windows 11's lock
# screen is an ordinary application on the Default desktop, so OpenInputDesktop answers "Default"
# for most of a lock and cannot tell a locked session from an unlocked one. This script asks it
# through its own P/Invoke so that the timeline it judges by is not produced by the code being
# judged.
# ---------------------------------------------------------------------------------------------
if (-not ('Probe.Session' -as [type])) {
    Add-Type -Namespace Probe -Name Session -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("wtsapi32.dll", SetLastError = true, CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
public static extern bool WTSQuerySessionInformationW(System.IntPtr server, uint session, int infoClass,
    out System.IntPtr buffer, out uint bytes);
[System.Runtime.InteropServices.DllImport("wtsapi32.dll")]
public static extern void WTSFreeMemory(System.IntPtr memory);
[System.Runtime.InteropServices.DllImport("user32.dll", SetLastError = true)]
public static extern bool LockWorkStation();
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern System.IntPtr SendMessageTimeout(System.IntPtr window, uint message, System.IntPtr wparam,
    System.IntPtr lparam, uint flags, uint timeout, out System.IntPtr result);
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, System.IntPtr extra);
'@
}

$WTS_CURRENT_SERVER = [System.IntPtr]::Zero
$WTS_CURRENT_SESSION = [uint32]::MaxValue
$WTSSessionInfoEx = 25
# WTSINFOEXW is { DWORD Level; <4 bytes of padding>; WTSINFOEX_LEVEL1_W Data; }, and level 1 opens
# with { ULONG SessionId; WTS_CONNECTSTATE_CLASS SessionState; LONG SessionFlags; }. The union is
# eight-byte aligned because it contains LARGE_INTEGERs, which is where the padding comes from.
$OFFSET_LEVEL = 0
$OFFSET_SESSION_ID = 8
$OFFSET_SESSION_FLAGS = 16
$WTS_SESSIONSTATE_LOCK = 0
$WTS_SESSIONSTATE_UNLOCK = 1

# Reads the whole of the header this script depends on, so a wrong offset is a caught error rather
# than a plausible answer. Returns $null when Windows would not answer.
function Get-SessionInfoEx {
    $buffer = [System.IntPtr]::Zero
    # Typed, not just initialized: the out-parameter is a ULONG, and a bare 0 is an Int32 that the
    # marshaller refuses outright.
    $bytes = [uint32] 0
    $queried = [Probe.Session]::WTSQuerySessionInformationW($WTS_CURRENT_SERVER, $WTS_CURRENT_SESSION,
        $WTSSessionInfoEx, [ref] $buffer, [ref] $bytes)
    if (-not $queried -or $buffer -eq [System.IntPtr]::Zero) { return $null }
    try {
        if ($bytes -lt ($OFFSET_SESSION_FLAGS + 4)) { return $null }
        return [pscustomobject]@{
            Level     = [System.Runtime.InteropServices.Marshal]::ReadInt32($buffer, $OFFSET_LEVEL)
            SessionId = [System.Runtime.InteropServices.Marshal]::ReadInt32($buffer, $OFFSET_SESSION_ID)
            Flags     = [System.Runtime.InteropServices.Marshal]::ReadInt32($buffer, $OFFSET_SESSION_FLAGS)
        }
    } finally {
        [Probe.Session]::WTSFreeMemory($buffer)
    }
}

function Get-SessionLock {
    $info = Get-SessionInfoEx
    if ($null -eq $info) { return 'unknown' }
    switch ($info.Flags) {
        $WTS_SESSIONSTATE_LOCK { 'locked' }
        $WTS_SESSIONSTATE_UNLOCK { 'unlocked' }
        # Windows Server 2008 R2 is documented to return the pair reversed, and nothing in the
        # answer says which convention produced it. Guessing a lock is worse than not answering.
        default { 'unknown' }
    }
}

# Proves the offsets above against facts this process already knows, while the session is unlocked
# and the answer is therefore verifiable. Without this, reading SessionState instead of
# SessionFlags would report an active session as "locked" and a run would judge by it.
function Assert-SessionInfoLayout {
    $info = Get-SessionInfoEx
    if ($null -eq $info) { throw 'WTSSessionInfoEx did not answer; this run has no lock timeline to judge by' }
    $expectedSession = (Get-Process -Id $PID).SessionId
    if ($info.Level -ne 1) {
        throw "WTSINFOEXW reported level $($info.Level); the struct offsets this script reads are for level 1"
    }
    if ($info.SessionId -ne $expectedSession) {
        throw "WTSINFOEXW session id read as $($info.SessionId) but this process is in session $expectedSession; the struct offsets are wrong"
    }
    Write-Line "session info layout verified: level $($info.Level), session $($info.SessionId), flags $($info.Flags)"
}

$HWND_BROADCAST = [System.IntPtr] 0xffff
$WM_SYSCOMMAND = 0x0112
$SC_MONITORPOWER = [System.IntPtr] 0xF170
$SMTO_ABORTIFHUNG = 0x0002
$MOUSEEVENTF_MOVE = 0x0001

# Asks every top-level window to power the displays down, in a background job.
#
# On its own process because a locked session suspends applications and this broadcast waits on
# each of them in turn: one run of the Rust harness spent four minutes inside it. In-process that
# would stall everything after it; here the worst case is a job that never finishes and a script
# that never notices.
function Start-MonitorPowerOff {
    Start-Job -ScriptBlock {
        Add-Type -Namespace Probe -Name Power -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern System.IntPtr SendMessageTimeout(System.IntPtr window, uint message, System.IntPtr wparam,
    System.IntPtr lparam, uint flags, uint timeout, out System.IntPtr result);
'@
        $result = [System.IntPtr]::Zero
        [void] [Probe.Power]::SendMessageTimeout([System.IntPtr] 0xffff, 0x0112, [System.IntPtr] 0xF170,
            [System.IntPtr] 2, 0x0002, 2000, [ref] $result)
        'broadcast returned'
    }
}

# A zero-delta move is real input as far as Windows is concerned, so it both wakes the panel and
# resets the idle timer. Only ever called on the way out, once the measurement is over.
function Invoke-DisplayWake {
    [Probe.Session]::mouse_event($MOUSEEVENTF_MOVE, 0, 0, 0, [System.IntPtr]::Zero)
    $result = [System.IntPtr]::Zero
    [void] [Probe.Session]::SendMessageTimeout($HWND_BROADCAST, $WM_SYSCOMMAND, $SC_MONITORPOWER,
        [System.IntPtr] -1, $SMTO_ABORTIFHUNG, 2000, [ref] $result)
    [Probe.Session]::mouse_event($MOUSEEVENTF_MOVE, 0, 0, 0, [System.IntPtr]::Zero)
}

# ---------------------------------------------------------------------------------------------
# The run
# ---------------------------------------------------------------------------------------------
if (-not (Test-Path $Exe)) {
    throw "$Exe is missing; build it first with: cargo build -p captastic-app"
}
Assert-SessionInfoLayout
if ((Get-SessionLock) -ne 'unlocked') {
    throw 'the workstation is already locked, or the lock query does not answer on this host; either way what follows would measure nothing'
}

# Captures are bounded by attempts rather than by time, because a capture dropped while the
# desktop is missing still spends one: at one a second, a budget sized for the baseline alone
# would be gone before the operator reached the kitchen. Sized for the whole run instead, so the
# daemon cannot outlive the measurement and the measurement cannot outlive the daemon.
$plannedSeconds = $BaselineSeconds + ($UnlockWaitMinutes * 60) + $AfterUnlockSeconds
$captures = [int][math]::Ceiling(($plannedSeconds * 1000.0) / $IntervalMs) + 60

Write-Line ("mode: " + $(if ($Confirmed) { 'LIVE - this run locks the workstation' } else { 'DRY RUN - nothing will be locked' }))
Write-Line "daemon: $Exe"
Write-Line "capture budget: $captures at ${IntervalMs}ms"
if ($Confirmed) {
    # Said here rather than in a comment, because this is the last thing the operator reads
    # before the screen goes away and there is nothing to read it on afterwards.
    ''
    ">>> This window locks the workstation in $BaselineSeconds seconds and powers the displays down."
    '>>> Leave the machine alone once it does: any input wakes the displays and ends the condition'
    '>>> being measured. Nothing here can unlock it - come back in at least ' +
        "$([int]($MinimumLockSeconds / 60)) minute(s) and sign in as usual."
    ">>> Stay away longer if you like; the run gives up after $UnlockWaitMinutes minutes and puts"
    '>>> your daemon back either way. The result is waiting in this window when you return.'
    ''
}

$userWasRunning = [bool] (Get-Process captastic -ErrorAction SilentlyContinue)
$daemon = $null
$powerJob = $null
$samples = New-Object System.Collections.Generic.List[object]
$lockIssuedUtc = $null
$lockedAtUtc = $null
$unlockedAtUtc = $null
$daemonAliveAtUnlock = $false
$gaveUp = $false
try {
    # The installed daemon is a pre-#70 build on this host and dies on a locked-session denial;
    # it also owns the hotkeys. Stopped for the run and put back in the finally, so a throw
    # anywhere in the window this script deliberately makes hostile cannot be the reason the
    # user's daemon stays down.
    if ($userWasRunning) {
        Write-Line 'stopping the installed daemon for the duration'
        & $UserExe stop 2>&1 | Out-Null
        Start-Sleep -Seconds 3
    }

    # Clipboard and selection off: this is about the capture engine's lifecycle, and a soak that
    # quietly replaces the user's clipboard is how their clipboard got destroyed twice before.
    $daemonArgs = @(
        '--log-file', $log, '--log-level', 'debug', '--log-format', 'compact',
        'daemon', '--backend', 'dxgi', '--mode', 'latest', '--cpu-frame', 'true',
        '--clipboard', 'false', '--selection', 'false',
        '--self-trigger', '--self-trigger-interval-ms', "$IntervalMs", '--max-captures', "$captures"
    )
    $daemon = Start-Process -FilePath $Exe -ArgumentList $daemonArgs -RedirectStandardOutput $out `
        -RedirectStandardError $err -PassThru -WindowStyle Hidden
    Start-Sleep -Seconds 3
    if ($daemon.HasExited) {
        Write-Line "daemon exited immediately with code $($daemon.ExitCode); aborting - see $err"
        return
    }
    Write-Line "daemon pid $($daemon.Id); baseline ${BaselineSeconds}s"

    'Utc,ElapsedSeconds,Phase,Lock,DaemonAlive' | Set-Content -Path $csv -Encoding UTF8

    $started = [DateTime]::UtcNow
    $hardDeadline = $started.AddSeconds($plannedSeconds + 120)
    # Counted as well as timed: a loop bounded only by the clock is one a stall can skip
    # entirely, and a loop that has sampled nothing reports that it measured nothing.
    $maxSamples = [int][math]::Ceiling((($plannedSeconds + 120) * 1000.0) / $SampleMs)
    $phase = 'baseline'
    $sampleCount = 0

    while ($sampleCount -lt $maxSamples -and [DateTime]::UtcNow -lt $hardDeadline) {
        $now = [DateTime]::UtcNow
        $elapsed = [math]::Round(($now - $started).TotalSeconds, 1)
        $lock = Get-SessionLock
        $alive = $null -ne (Get-Process -Id $daemon.Id -ErrorAction SilentlyContinue)

        $samples.Add([pscustomobject]@{ Utc = $now; State = $lock })
        '{0},{1},{2},{3},{4}' -f $now.ToString('yyyy-MM-ddTHH:mm:ss.ffffffZ'), $elapsed, $phase, $lock, $alive |
            Add-Content -Path $csv -Encoding UTF8
        # Written to the console every sample, not just on a change, and this is load-bearing
        # rather than decorative: `latest` captures a *new* frame, so a desktop where nothing
        # moves produces DXGI_ERROR_WAIT_TIMEOUT rather than a screenshot. A line a second in a
        # visible terminal is enough for the display showing it to keep producing frames, which is
        # what gives the run a baseline and a post-unlock recovery made of successes rather than
        # timeouts. It also tells the returning operator the run is still going.
        Write-Host ("  [{0,6}s] {1,-12} lock={2}" -f $elapsed, $phase, $lock)

        # This host locks itself when left alone - it did so in the middle of a dry run - and a
        # baseline taken against a lock screen is not a baseline. Better to say so than to measure
        # a recovery from a lock that was never this run's to cause.
        if ($phase -eq 'baseline' -and $lock -ne 'unlocked') {
            throw "the session went $lock during the baseline, before this run locked anything; the baseline would be worthless. Try again with the machine in use"
        }

        if ($phase -eq 'baseline' -and $elapsed -ge $BaselineSeconds) {
            if (-not $Confirmed) {
                Write-Line 'DRY RUN: this is where the workstation would lock; skipping'
                $phase = 'dry-run-tail'
                $unlockedAtUtc = $now
            } else {
                Write-Line 'LOCKING the workstation now'
                $lockIssuedUtc = [DateTime]::UtcNow
                if (-not [Probe.Session]::LockWorkStation()) {
                    throw 'LockWorkStation failed; this run cannot measure a lock it could not cause'
                }
                $phase = 'locking'
            }
        }
        # Duplication keeps working while the lock screen is lit - twelve seconds of it in one
        # run - and it is the power-down that ends it. Without this the run would mostly sample
        # the state that was never broken.
        if ($phase -eq 'locking' -and ($now - $lockIssuedUtc).TotalSeconds -ge 3) {
            Write-Line 'asking the displays to power down'
            $powerJob = Start-MonitorPowerOff
            $phase = 'locked'
        }
        # Recorded from the flag rather than from the phase: the phase spends three seconds
        # waiting to power the displays down, and dating the lock from the end of that would
        # shorten every duration this run reports.
        if ($null -ne $lockIssuedUtc -and $null -eq $lockedAtUtc -and $lock -eq 'locked') {
            $lockedAtUtc = $now
            Write-Line "lock flag confirmed at ${elapsed}s"
        }
        if ($null -ne $lockedAtUtc -and $null -eq $unlockedAtUtc -and $lock -eq 'unlocked') {
            $unlockedAtUtc = $now
            $daemonAliveAtUnlock = $alive
            $lockedSeconds = [math]::Round(($now - $lockedAtUtc).TotalSeconds, 1)
            Write-Line "UNLOCKED after ${lockedSeconds}s locked; sampling for a further ${AfterUnlockSeconds}s"
            $phase = 'after-unlock'
        }
        if ($phase -in @('after-unlock', 'dry-run-tail') -and ($now - $unlockedAtUtc).TotalSeconds -ge $AfterUnlockSeconds) {
            Write-Line 'post-unlock window complete'
            break
        }
        if ($null -ne $lockedAtUtc -and $null -eq $unlockedAtUtc -and ($now - $lockedAtUtc).TotalSeconds -ge ($UnlockWaitMinutes * 60)) {
            Write-Line "still locked after $UnlockWaitMinutes minutes; giving up and restoring"
            $gaveUp = $true
            break
        }
        if (-not $alive) {
            Write-Line 'the daemon is gone; stopping'
            break
        }

        $sampleCount++
        Start-Sleep -Milliseconds $SampleMs
    }
    if ($sampleCount -ge $maxSamples) {
        Write-Line 'sample budget spent before the run finished'
        $gaveUp = $true
    }
} finally {
    # Belt and braces: whatever happened above, the panels must be on when this exits.
    try { Invoke-DisplayWake } catch { Write-Line "display wake failed: $_" }
    if ($powerJob) { Remove-Job -Job $powerJob -Force -ErrorAction SilentlyContinue }
    if ($daemon -and -not $daemon.HasExited) {
        & $Exe stop 2>&1 | Out-Null
        Start-Sleep -Seconds 5
        if (-not $daemon.HasExited) { Stop-Process -Id $daemon.Id -Force }
    }
    if ($userWasRunning) {
        # The same call install.ps1 makes: captastic-desktop.exe is the entry point that brings the
        # tray daemon back, and it runs from its own directory.
        $tools = Split-Path $UserExe -Parent
        Start-Process -FilePath (Join-Path $tools 'captastic-desktop.exe') -WorkingDirectory $tools -WindowStyle Hidden
        Start-Sleep -Seconds 4
        $back = Get-Process captastic -ErrorAction SilentlyContinue
        Write-Line ('installed daemon: ' + $(if ($back) { "restarted (pid $($back.Id))" } else { 'NOT RUNNING - restart it by hand' }))
    } else {
        Write-Line 'installed daemon: was not running before the run; nothing to restore'
    }
}

# ---------------------------------------------------------------------------------------------
# Reading the log back
# ---------------------------------------------------------------------------------------------
if (-not (Test-Path $log)) {
    Write-Line "no log was written to $log; check $err"
    exit 1
}

# ToArray rather than @(): PowerShell's array-subexpression binder refuses a
# List[object] of PSCustomObjects outright, with an "Argument types do not match" that names
# nothing useful.
$sampleList = $samples.ToArray()
# The lock flag is read either side of every log line and only lines where both reads agree are
# judged. A lock has three phases and the session moves between them while this runs; a line
# timestamped across a move describes no single moment, and judging one reports the unlock
# transition as though it were the condition under test - which is exactly what an earlier
# harness did, on four samples out of a hundred. Callers must ask in chronological order.
$script:cursor = 0
function Get-LockAt([datetime] $at) {
    if ($sampleList.Count -lt 2 -or $at -lt $sampleList[0].Utc) { return 'outside' }
    while ($script:cursor -lt ($sampleList.Count - 2) -and $sampleList[$script:cursor + 1].Utc -le $at) {
        $script:cursor++
    }
    $before = $sampleList[$script:cursor]
    $after = $sampleList[$script:cursor + 1]
    if ($at -lt $before.Utc -or $at -gt $after.Utc) { return 'outside' }
    if ($before.State -eq $after.State) { return $before.State }
    return 'transition'
}

# Timeout is called out separately from every other failure because it means the opposite thing.
# A capture can only time out on a duplication the daemon is holding: it says the engine is alive
# and the desktop simply did not redraw. Counting it as a failure would report a static lock
# screen as a broken engine, and counting it as a success would report one as a screenshot.
$patterns = [ordered]@{
    Success       = 'capture \d+ action=\S+: native '
    Timeout       = 'capture \d+ action=\S+ failed: Timeout in '
    Dropped       = 'capture \d+ ignored: there is no display to capture yet'
    Recovering    = 'capture \d+ ignored while the capture engine is recovering'
    Failed        = 'capture \d+ action=\S+ failed: (?!Timeout in )'
    LostEngine    = 'lost the capture engine'
    ReinitFailed  = 'capture engine reinitialization failed'
    StillWaiting  = 'still waiting for an interactive desktop'
    Heartbeat     = 'still waiting for a capture source: '
    Recovered     = 'a display is available again; capture engine initialized'
    EngineBack    = 'capture engine recovered during capture'
    Invalidated   = 'display configuration invalidated'
}

$tally = @{}
foreach ($name in $patterns.Keys) {
    $tally[$name] = @{ preLock = 0; locked = 0; postUnlock = 0; unjudged = 0; total = 0 }
}
$notable = New-Object System.Collections.Generic.List[string]
$failureKinds = @{}
$heartbeatWalks = New-Object System.Collections.Generic.List[object]
$recoveredAtUtc = $null
$recoveredTallyText = $null
$firstSuccessAfterUnlock = $null
$lastLostEngineUtc = $null

foreach ($line in (Get-Content -Path $log)) {
    if (-not ($line -match '^(?<ts>\S+Z)\s')) { continue }
    $at = ([datetime]::Parse($matches.ts, [cultureinfo]::InvariantCulture,
        [System.Globalization.DateTimeStyles]::RoundtripKind)).ToUniversalTime()
    $label = Get-LockAt $at
    $bucket = if ($null -ne $lockIssuedUtc -and $at -lt $lockIssuedUtc) {
        'preLock'
    } elseif ($null -eq $lockIssuedUtc) {
        'preLock'
    } elseif ($label -eq 'locked') {
        'locked'
    } elseif ($label -eq 'unlocked' -and $null -ne $unlockedAtUtc -and $at -ge $unlockedAtUtc) {
        'postUnlock'
    } else {
        'unjudged'
    }

    foreach ($name in $patterns.Keys) {
        if ($line -match $patterns[$name]) {
            $tally[$name][$bucket]++
            $tally[$name]['total']++
            switch ($name) {
                'Success' {
                    if ($bucket -eq 'postUnlock' -and $null -eq $firstSuccessAfterUnlock) {
                        $firstSuccessAfterUnlock = $at
                    }
                }
                'Failed' {
                    if ($line -match 'failed: (?<kind>\w+) in ') {
                        $key = "$bucket/$($matches.kind)"
                        if (-not $failureKinds.ContainsKey($key)) { $failureKinds[$key] = 0 }
                        $failureKinds[$key]++
                    }
                }
                'LostEngine' { $lastLostEngineUtc = $at; $notable.Add($line) }
                'ReinitFailed' { $notable.Add($line) }
                'StillWaiting' { $notable.Add($line) }
                'Recovered' {
                    $recoveredAtUtc = $at
                    $notable.Add($line)
                    if ($line -match '(?<polls>\d+) source poll\(s\) over (?<secs>[\d.]+) s, (?<walks>\d+) of which walked') {
                        $recoveredTallyText = "$($matches.polls) polls over $($matches.secs) s, $($matches.walks) adapter walk(s)"
                    }
                }
                'Heartbeat' {
                    if ($line -match '(?<polls>\d+) source poll\(s\) over (?<secs>[\d.]+) s, (?<walks>\d+) of which walked') {
                        $heartbeatWalks.Add([pscustomobject]@{
                            Utc = $at; Bucket = $bucket; Polls = [int] $matches.polls; Walks = [int] $matches.walks
                        })
                    }
                    $notable.Add($line)
                }
                'Invalidated' { $notable.Add($line) }
                'EngineBack' { $notable.Add($line) }
            }
        }
    }
}

''
'--- what the daemon logged ---'
$notable | ForEach-Object { $_ }
''
'--- counts, by where the lock flag agreed either side of the line ---'
'{0,-14} {1,8} {2,8} {3,11} {4,9}' -f 'event', 'pre-lock', 'locked', 'post-unlock', 'unjudged'
foreach ($name in $patterns.Keys) {
    if ($tally[$name]['total'] -eq 0) { continue }
    '{0,-14} {1,8} {2,8} {3,11} {4,9}' -f $name, $tally[$name]['preLock'], $tally[$name]['locked'],
        $tally[$name]['postUnlock'], $tally[$name]['unjudged']
}
''
"lock samples: $($sampleList.Count) at ${SampleMs}ms"
if ($lockedAtUtc -and $unlockedAtUtc) {
    "locked for: {0:N1} s" -f ($unlockedAtUtc - $lockedAtUtc).TotalSeconds
}
if ($recoveredTallyText) { "wait tally on recovery: $recoveredTallyText" }
if ($unlockedAtUtc -and $recoveredAtUtc) {
    "unlock to engine rebuilt: {0:N1} s" -f ($recoveredAtUtc - $unlockedAtUtc).TotalSeconds
}
if ($unlockedAtUtc -and $firstSuccessAfterUnlock) {
    "unlock to first successful capture: {0:N1} s" -f ($firstSuccessAfterUnlock - $unlockedAtUtc).TotalSeconds
}
foreach ($beat in $heartbeatWalks) {
    "heartbeat [{0}]: {1} polls, {2} adapter walk(s)" -f $beat.Bucket, $beat.Polls, $beat.Walks
}
foreach ($key in ($failureKinds.Keys | Sort-Object)) {
    "capture failures [{0}]: {1}" -f $key, $failureKinds[$key]
}

# ---------------------------------------------------------------------------------------------
# The verdict
# ---------------------------------------------------------------------------------------------
''
if (-not $Confirmed) {
    '--- DRY RUN ---'
    "the workstation was never locked, so nothing about lock/unlock recovery was measured."
    $ok = $tally['Success']['preLock'] -gt 0 -or $tally['Success']['total'] -gt 0
    "plumbing: daemon started, {0} capture(s) logged, log parsed, installed daemon restored" -f $tally['Success']['total']
    "re-run with -Confirmed to measure the real thing."
    exit $(if ($ok) { 0 } else { 1 })
}

$verdicts = New-Object System.Collections.Generic.List[object]
function Add-Verdict([string] $name, [bool] $passed, [string] $detail) {
    $verdicts.Add([pscustomobject]@{ Name = $name; Passed = $passed; Detail = $detail })
}

$lockedHeartbeats = @($heartbeatWalks | Where-Object { $_.Bucket -eq 'locked' })
$walkedWhileLocked = @($lockedHeartbeats | Where-Object { $_.Walks -gt 0 })
$measuredWhileLocked = $tally['Dropped']['locked'] + $tally['Failed']['locked'] + $tally['LostEngine']['locked']
$noRelapse = ($null -eq $lastLostEngineUtc) -or ($null -ne $recoveredAtUtc -and $lastLostEngineUtc -le $recoveredAtUtc)
# Both of these reached a live duplication. Only the successes carry pixels, but a timeout is
# still the engine working, and a run whose desktop happened to sit still afterwards should not
# read as a daemon that never came back.
$reachedAfter = $tally['Success']['postUnlock'] + $tally['Timeout']['postUnlock']
$refusedAfter = $tally['Dropped']['postUnlock'] + $tally['Recovering']['postUnlock']

Add-Verdict 'baseline captures' ($tally['Success']['preLock'] -ge 5) `
    "$($tally['Success']['preLock']) successful capture(s) before the lock; a run with no baseline has lost nothing"
Add-Verdict 'daemon survived the lock' ($daemonAliveAtUnlock -and -not $gaveUp) `
    $(if ($gaveUp) { 'the run gave up before an unlock was seen' } elseif ($daemonAliveAtUnlock) { 'the daemon was still running when the session unlocked' } else { 'the daemon was gone by the time the session unlocked' })
Add-Verdict 'the lock was measured' ($measuredWhileLocked -gt 0) `
    ("$measuredWhileLocked capture(s) were refused or dropped while the lock flag agreed either side, alongside $($tally['Timeout']['locked']) timeout(s). " +
     'It is the display power-down rather than the lock that ends duplication, so zero refusals with a healthy run of timeouts is a lock whose monitors stayed awake - or a held duplication that never noticed they had not, which is a finding rather than a harness fault. Zero of both is a run that measured nothing at all')
Add-Verdict 'the refusal was explained' ($tally['StillWaiting']['total'] -gt 0) `
    "$($tally['StillWaiting']['total']) line(s) classified the refused rebuild as a missing desktop rather than a broken engine"
Add-Verdict 'no adapter walk while locked' (($lockedHeartbeats.Count -gt 0) -and ($walkedWhileLocked.Count -eq 0)) `
    "$($lockedHeartbeats.Count) heartbeat(s) inside the lock, $($walkedWhileLocked.Count) of which had walked the adapter list. Zero heartbeats means the lock was too short to have measured this"
Add-Verdict 'the engine came back' (($null -ne $recoveredAtUtc) -and ($null -ne $unlockedAtUtc) -and ($recoveredAtUtc -ge $unlockedAtUtc)) `
    $(if ($recoveredAtUtc) { "the engine was rebuilt at $($recoveredAtUtc.ToString('HH:mm:ss'))" } else { 'the engine was never rebuilt; this is the finding the exercise exists to look for' })
Add-Verdict 'the engine is live again' (($reachedAfter -ge 5) -and ($refusedAfter -eq 0)) `
    "$reachedAfter capture(s) reached the duplication after the unlock and $refusedAfter were still refused for a missing desktop"
Add-Verdict 'frames flowed again' ($tally['Success']['postUnlock'] -ge 1) `
    "$($tally['Success']['postUnlock']) capture(s) came back with pixels after the unlock. A failure here alongside a live engine means the desktop never redrew, which latest-mode reports as a timeout - check the run had a visible terminal"
Add-Verdict 'and stayed resumed' $noRelapse `
    $(if ($noRelapse) { 'no further engine loss after the rebuild' } else { 'the engine was lost again after it came back, which is a recovery that did not hold' })

'--- verdict ---'
foreach ($verdict in $verdicts) {
    '{0}  {1,-28} {2}' -f $(if ($verdict.Passed) { 'PASS' } else { 'FAIL' }), $verdict.Name, $verdict.Detail
}
''
$failed = @($verdicts | Where-Object { -not $_.Passed })
if ($lockedAtUtc -and $unlockedAtUtc -and ($unlockedAtUtc - $lockedAtUtc).TotalSeconds -lt $MinimumLockSeconds) {
    "NOTE: the lock lasted less than the ${MinimumLockSeconds}s asked for, so a thin run here is the operator returning early rather than a regression."
}
"artifacts: $log, $csv, $runLog"
if ($failed.Count -eq 0) {
    'RESULT: the daemon lost its capture engine to a locked, dark session and recovered on its own after the unlock.'
    exit 0
}
"RESULT: $($failed.Count) of $($verdicts.Count) judgments failed. Do not loosen them; report what happened."
exit 1
