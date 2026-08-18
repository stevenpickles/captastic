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
    # Any leg set to zero minutes is skipped, so one harness can run a whole investigation or a
    # single follow-up without editing it.
    [int] $IdleMinutes = 8,
    [int] $BusyMinutes = 40,
    [int] $TriggerIntervalMs = 500,
    # The clipboard leg restores two of the original run's conditions at once: the clipboard
    # destination, and the BufferExhausted refusals that a 250 ms interval produces at 4K.
    [int] $ClipboardMinutes = 0,
    [int] $ClipboardIntervalMs = 250,
    # The full original configuration: both destinations at once. The only way to reach the
    # BufferExhausted path, which needs the two of them contending for the three CPU pool slots.
    [int] $BothMinutes = 0,
    [int] $BothIntervalMs = 250,
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

# Saves whatever the user has on the clipboard, so a soak that publishes thousands of captures to
# it can put it back. This is not optional politeness: the clipboard leg overwrites the clipboard
# roughly once a second for its whole duration, and an earlier soak destroyed a real clipboard
# twice - once because no backup was taken, and once because the backup was taken before the
# *previous* leg and had already been consumed. Re-save immediately before every leg that touches
# it, never once at the top of a run.
function Save-Clipboard([string] $Directory) {
    Add-Type -AssemblyName System.Windows.Forms
    Add-Type -AssemblyName System.Drawing
    $saved = @{ Text = $null; Html = $null; ImagePath = $null; Formats = @() }
    try {
        $data = [System.Windows.Forms.Clipboard]::GetDataObject()
        if ($null -eq $data) { return $saved }
        $saved.Formats = @($data.GetFormats())
        if ($data.GetDataPresent('UnicodeText')) { $saved.Text = [string] $data.GetData('UnicodeText') }
        if ($data.GetDataPresent('HTML Format')) { $saved.Html = [string] $data.GetData('HTML Format') }
        if ([System.Windows.Forms.Clipboard]::ContainsImage()) {
            $image = [System.Windows.Forms.Clipboard]::GetImage()
            if ($image) {
                $saved.ImagePath = Join-Path $Directory 'clipboard-backup.png'
                $image.Save($saved.ImagePath, [System.Drawing.Imaging.ImageFormat]::Png)
                $image.Dispose()
            }
        }
    } catch {
        Write-Line "WARNING: could not fully read the clipboard to back it up: $_"
    }
    return $saved
}

function Restore-Clipboard($Saved) {
    if (-not $Saved -or (-not $Saved.Text -and -not $Saved.Html -and -not $Saved.ImagePath)) {
        Write-Line 'clipboard held nothing restorable; leaving the capture on it'
        return
    }
    try {
        if ($Saved.ImagePath -and (Test-Path $Saved.ImagePath)) {
            $image = [System.Drawing.Image]::FromFile($Saved.ImagePath)
            [System.Windows.Forms.Clipboard]::SetImage($image)
            $image.Dispose()
            Write-Line 'clipboard restored (image)'
            return
        }
        $object = New-Object System.Windows.Forms.DataObject
        if ($Saved.Text) { $object.SetData('UnicodeText', $Saved.Text) }
        if ($Saved.Html) { $object.SetData('HTML Format', $Saved.Html) }
        [System.Windows.Forms.Clipboard]::SetDataObject($object, $true)
        Write-Line "clipboard restored (text $($Saved.Text.Length) chars)"
    } catch {
        Write-Line "WARNING: clipboard restore FAILED: $_"
    }
}

# Runs one leg: start a daemon, sample it for the duration, stop it, and keep everything.
function Invoke-Leg([string] $Name, [int] $Minutes, [string[]] $ExtraArgs) {
    $daemonOut = Join-Path $OutDir "$Name-daemon.log"
    $csv = Join-Path $OutDir "$Name-resources.csv"
    # Destinations are the caller's business: each leg exists to turn exactly one of them on or off.
    $arguments = @('daemon', '--backend', 'dxgi', '--selection', 'false') + $ExtraArgs
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
    if ($IdleMinutes -gt 0) {
        Invoke-Leg -Name 'idle' -Minutes $IdleMinutes -ExtraArgs @('--clipboard', 'false')
    }

    # Treatment: capture work, with the two destinations from the original run removed.
    if ($BusyMinutes -gt 0) {
        Invoke-Leg -Name 'busy' -Minutes $BusyMinutes -ExtraArgs @(
            '--clipboard', 'false',
            '--self-trigger', '--self-trigger-interval-ms', "$TriggerIntervalMs"
        )
    }

    # Reproduction attempt: the clipboard destination back on, at the interval that produced the
    # original run's 787 BufferExhausted refusals. Runs against a throwaway configuration so the
    # user's captastic.toml and state.toml are not touched, and with clipboard retention allowed,
    # because the original predates the opt-out added in #54 - a faithful reproduction has to
    # include the format synthesis that a retained clipboard capture can provoke.
    if ($ClipboardMinutes -gt 0) {
        $configPath = Join-Path $OutDir 'soak-config.toml'
        @'
schema_version = 1

[daemon]
backend = "dxgi"
display = "primary"

[capture]
mode = "latest"
cpu_frame = true

[clipboard]
enabled = true
queue_capacity = 1
allow_history = true
allow_cloud_sync = true

[output]
enabled = false

[selection]
enabled = false
'@ | Set-Content -Path $configPath -Encoding UTF8
        $backup = Save-Clipboard -Directory $OutDir
        Write-Line "clipboard backed up before the leg (formats: $($backup.Formats -join ', '))"
        try {
            Invoke-Leg -Name 'clipboard' -Minutes $ClipboardMinutes -ExtraArgs @(
                '--config', $configPath,
                '--clipboard', 'true',
                '--self-trigger', '--self-trigger-interval-ms', "$ClipboardIntervalMs"
            )
        }
        finally {
            Restore-Clipboard -Saved $backup
        }
    }

    # The original configuration, reproduced whole: clipboard and file output together at 250 ms.
    # Captures go to a scratch directory that is measured and deleted afterwards - roughly 3 MB
    # each, so this is the one leg with a disk cost worth stating before it runs.
    if ($BothMinutes -gt 0) {
        $captureDir = Join-Path $OutDir 'captures'
        New-Item -ItemType Directory -Force $captureDir | Out-Null
        $configPath = Join-Path $OutDir 'both-config.toml'
        $toml = @"
schema_version = 1

[daemon]
backend = "dxgi"
display = "primary"

[capture]
mode = "latest"
cpu_frame = true

[clipboard]
enabled = true
queue_capacity = 1
allow_history = true
allow_cloud_sync = true

[output]
enabled = true
format = "png"
queue_capacity = 2
directory = '$captureDir'
filename_template = "{timestamp}"

[selection]
enabled = false
"@
        Set-Content -Path $configPath -Value $toml -Encoding UTF8
        $backup = Save-Clipboard -Directory $OutDir
        Write-Line "clipboard backed up before the both leg (formats: $($backup.Formats -join ', '))"
        try {
            Invoke-Leg -Name 'both' -Minutes $BothMinutes -ExtraArgs @(
                '--config', $configPath,
                '--clipboard', 'true',
                '--self-trigger', '--self-trigger-interval-ms', "$BothIntervalMs"
            )
        }
        finally {
            Restore-Clipboard -Saved $backup
            $files = @(Get-ChildItem -Path $captureDir -Filter *.png -ErrorAction SilentlyContinue)
            $bytes = ($files | Measure-Object -Property Length -Sum).Sum
            Write-Line ("captures written: {0} files, {1:N2} GB" -f $files.Count, ($bytes / 1GB))
            Remove-Item -Path $captureDir -Recurse -Force -ErrorAction SilentlyContinue
            Write-Line 'capture directory deleted'
        }
    }
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
