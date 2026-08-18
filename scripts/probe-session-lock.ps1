<#
.SYNOPSIS
    Measures what a lock/unlock transition does to the daemon's GDI, USER and handle counts.

.DESCRIPTION
    Four soak runs eliminated every Captastic-side suspect for the step in issue #53: the capture
    path, both destinations, the BufferExhausted refusal path, and display power. 12,472 captures
    with GDI at exactly 10 in every sample of every run.

    One environmental candidate is left, and it is the one that actually happened during the
    original run - the workstation locked itself partway through the endurance soak. A lock
    transition is a plausible source of a one-time batch of USER and GDI objects in a process that
    owns a tray icon and a message-only window, and it fits the shape of the observation exactly:
    one step, then flat forever regardless of subsequent work. The lock test run for issue #51
    never sampled these counters, so this has never been measured.

    Captures run throughout, so the same run also covers the lock/unlock half of Milestone 5's
    lifecycle-recovery bullet, and enumeration is sampled alongside - a locked session is where
    issue #51 saw an empty display list, and continuous sampling would catch it.

    Waits for the operator to lock the screen; everything after that is unattended.
#>
[CmdletBinding()]
param(
    [string] $Exe = 'C:\Users\Steven\work\captastic\target\release\captastic.exe',
    [string] $UserExe = 'C:\ProgramData\chocolatey\lib\captastic\tools\captastic\captastic.exe',
    [Parameter(Mandatory)] [string] $OutDir,
    [int] $LockWaitMinutes = 10,
    [int] $UnlockWaitMinutes = 20,
    [int] $AfterUnlockSeconds = 60,
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
[System.Runtime.InteropServices.DllImport("user32.dll", SetLastError = true)]
public static extern System.IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
[System.Runtime.InteropServices.DllImport("user32.dll", SetLastError = true)]
public static extern bool CloseDesktop(System.IntPtr desktop);
[System.Runtime.InteropServices.DllImport("user32.dll", SetLastError = true, CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
public static extern bool GetUserObjectInformationW(System.IntPtr obj, int index, System.Text.StringBuilder info, uint length, out uint needed);
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern uint GetGuiResources(System.IntPtr process, uint flags);
"@
Add-Type -Namespace Probe -Name Lock -MemberDefinition $nativeSource

# The desktop that owns input names the session state: Default while the user is signed in,
# refused outright once the lock screen takes over from a process running as that user.
function Get-InputDesktop {
    $handle = [Probe.Lock]::OpenInputDesktop(0, $false, 0x0001)
    if ($handle -eq [System.IntPtr]::Zero) { return 'refused' }
    $builder = New-Object System.Text.StringBuilder 256
    $needed = 0
    $name = if ([Probe.Lock]::GetUserObjectInformationW($handle, 2, $builder, 512, [ref] $needed)) {
        $builder.ToString()
    } else {
        'unnamed'
    }
    [void] [Probe.Lock]::CloseDesktop($handle)
    return $name
}

function Get-DisplayIdentity {
    try {
        $json = & $Exe displays --backend dxgi --json 2>$null | Out-String
        $displays = @($json | ConvertFrom-Json -ErrorAction Stop)
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

    $arguments = @('daemon', '--backend', 'dxgi', '--clipboard', 'false', '--selection', 'false',
        '--self-trigger', '--self-trigger-interval-ms', '2000')
    $daemon = Start-Process -FilePath $Exe -ArgumentList $arguments -RedirectStandardOutput "$daemonLog.out" -RedirectStandardError $daemonLog -PassThru -WindowStyle Hidden
    Start-Sleep -Seconds 3
    if ($daemon.HasExited) {
        Write-Line "daemon exited immediately with code $($daemon.ExitCode); aborting"
        return
    }
    Write-Line "daemon pid $($daemon.Id); waiting for the screen to be locked"

    'Timestamp,ElapsedSeconds,Phase,InputDesktop,Handles,GdiObjects,UserObjects,PrivateMB,DisplayCount,DisplayId' |
        Set-Content -Path $csv -Encoding UTF8

    $started = Get-Date
    $lockDeadline = $started.AddMinutes($LockWaitMinutes)
    $phase = 'baseline'
    $unlockedAt = $null
    $iteration = 0
    $previousDesktop = ''

    while ($true) {
        $now = Get-Date
        $elapsed = [int] ($now - $started).TotalSeconds
        $desktop = Get-InputDesktop
        $locked = ($desktop -ne 'Default')

        if ($desktop -ne $previousDesktop) {
            Write-Line "input desktop: $desktop"
            $previousDesktop = $desktop
        }

        if ($phase -eq 'baseline' -and $locked) {
            Write-Line 'LOCKED - sampling through the locked session'
            $phase = 'locked'
        } elseif ($phase -eq 'locked' -and -not $locked) {
            Write-Line "UNLOCKED - sampling for a further ${AfterUnlockSeconds}s"
            $phase = 'after-unlock'
            $unlockedAt = $now
        }

        $process = Get-Process -Id $daemon.Id -ErrorAction SilentlyContinue
        if ($null -eq $process) { Write-Line 'daemon vanished'; break }
        $gdi = [Probe.Lock]::GetGuiResources($process.Handle, 0)
        $user = [Probe.Lock]::GetGuiResources($process.Handle, 1)
        $privateMb = [math]::Round($process.PrivateMemorySize64 / 1MB, 2)

        # Enumeration is queried sparsely - it spawns a process - but often enough that an empty
        # display list during the lock could not slip between samples.
        $identity = @{ Count = ''; Id = '' }
        if ($iteration % 3 -eq 0) { $identity = Get-DisplayIdentity }

        '{0},{1},{2},{3},{4},{5},{6},{7},{8},{9}' -f $now.ToString('HH:mm:ss'), $elapsed, $phase, $desktop,
            $process.HandleCount, $gdi, $user, $privateMb, $identity.Count, $identity.Id |
            Add-Content -Path $csv -Encoding UTF8

        if ($phase -eq 'baseline' -and $now -gt $lockDeadline) {
            Write-Line "no lock within $LockWaitMinutes minutes; giving up"
            break
        }
        if ($phase -eq 'locked' -and $now -gt $started.AddMinutes($LockWaitMinutes + $UnlockWaitMinutes)) {
            Write-Line 'still locked after the unlock deadline; giving up'
            break
        }
        if ($phase -eq 'after-unlock' -and ($now - $unlockedAt).TotalSeconds -ge $AfterUnlockSeconds) {
            Write-Line 'post-unlock window complete'
            break
        }

        $iteration++
        Start-Sleep -Seconds $SampleSeconds
    }
}
finally {
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
