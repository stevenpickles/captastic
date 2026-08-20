# Measures what a real GPU device loss does to the running daemon, and whether it recovers alone.
#
# The unit harness in `dxgi.rs` (a_real_device_loss_routes_through_the_rebuild_path) measures raw
# duplication. This measures the product: the daemon's own classification, its retry budget, its
# back-off, and the notification-area outage notice. Nothing is faked - the operator causes a real
# device loss while the daemon is capturing twice a second, and the daemon's log is the evidence.
#
# The trigger is the operator's. This script never presses anything and never disables a device;
# it starts the daemon, tells the operator when to act, and reads the log afterwards. See
# docs/windows-backend.md for the candidate triggers and what each one costs.
#
# Expected, in order:
#   1. the daemon starts and self-triggers a capture every --IntervalMs
#   2. the operator causes a device loss partway through
#   3. the log names the failure: "lost the capture engine; dropping it and retrying k/3 in N ms"
#   4. the log says it came back: "capture engine recovered during capture N after k attempt(s)"
#   5. captures continue to the end of --Captures, and the daemon exits on its own
#
# A run that reaches (3) and never reaches (4) is the finding this exercise exists to look for.
param(
    # The repository this script lives in, rather than one machine's home directory.
    [string] $Repo = (Split-Path $PSScriptRoot -Parent),
    [Parameter(Mandatory = $true)]
    [string] $OutDir,
    # 500 ms is fast enough that a driver restart cannot slip between two captures, and slow
    # enough that the run is not a duplication soak with a device loss somewhere in it.
    [int] $IntervalMs = 500,
    # Captures, not seconds: the run is bounded by attempts so that a device loss that makes DXGI
    # calls cost seconds cannot silently shorten the measurement. 240 is about four minutes at the
    # default interval, which is plenty of time to act.
    [int] $Captures = 240,
    # How far into the run to tell the operator to trigger, leaving healthy samples either side.
    [int] $TriggerAfterSeconds = 20,
    [string] $UserExe = 'C:\ProgramData\chocolatey\lib\captastic\tools\captastic\captastic.exe'
)

$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $OutDir | Out-Null
$log = Join-Path $OutDir 'daemon.log'
$out = Join-Path $OutDir 'daemon.out'
$err = Join-Path $OutDir 'daemon.err'

$exe = Join-Path $Repo 'target\debug\captastic.exe'
if (-not (Test-Path $exe)) {
    throw "$exe is missing; build it first with: cargo build -p captastic-app"
}

# The installed daemon owns the hotkeys and would compete for them. Stopped for the run and put
# back afterwards, exactly as verify-no-display-recovery.ps1 does.
$userDaemon = Get-Process captastic -ErrorAction SilentlyContinue
if ($userDaemon) {
    & $UserExe stop 2>&1 | Out-Null
    Start-Sleep -Seconds 2
}

# Clipboard and selection off: this is about the capture engine's lifecycle, and a soak that
# quietly replaces the user's clipboard is how their clipboard got destroyed twice before.
$daemonArgs = @(
    '--log-file', $log, '--log-level', 'debug', '--log-format', 'compact',
    'daemon', '--backend', 'dxgi', '--mode', 'latest', '--cpu-frame', 'true',
    '--clipboard', 'false', '--selection', 'false',
    '--self-trigger', '--self-trigger-interval-ms', "$IntervalMs", '--max-captures', "$Captures"
)
$process = Start-Process -FilePath $exe -ArgumentList $daemonArgs -RedirectStandardOutput $out `
    -RedirectStandardError $err -PassThru -WindowStyle Hidden

Start-Sleep -Seconds $TriggerAfterSeconds
''
'>>> The daemon is capturing. Cause the device loss NOW, then leave the machine alone.'
'>>>   driver restart:  press Ctrl+Win+Shift+B'
'>>>   adapter cycle:   see docs/windows-backend.md; needs an elevated shell that survives a black screen'
''

# Bounded by the run's own length plus slack for a device loss that makes every DXGI call slow.
$budget = [int](($Captures * $IntervalMs) / 1000) + 120
$deadline = (Get-Date).AddSeconds($budget)
while (-not $process.HasExited -and (Get-Date) -lt $deadline) { Start-Sleep -Seconds 1 }
if (-not $process.HasExited) {
    & $exe stop 2>&1 | Out-Null
    Start-Sleep -Seconds 5
    if (-not $process.HasExited) { Stop-Process -Id $process.Id -Force }
}

if ($userDaemon) {
    Start-Process -FilePath $UserExe -WindowStyle Hidden
    Start-Sleep -Seconds 4
}
$back = Get-Process captastic -ErrorAction SilentlyContinue

"exit code: $($process.ExitCode)"
"user's daemon: " + $(if ($back) { "running (pid $($back.Id))" } else { 'NOT RUNNING' })
"log: $log"
''
'--- recovery seam ---'
# Every line the recovery path can emit, in the order the daemon wrote them. Anything absent is
# as informative as anything present, so the pattern is deliberately wide rather than a grep for
# the one line a passing run would produce.
$patterns = @(
    'lost the capture engine',
    'capture engine recovered',
    'capture engine reinitializ',
    'waiting for a display',
    'still waiting for an interactive desktop',
    'a display is available again',
    'no display to capture',
    'while the capture engine is recovering',
    'failed:'
)
if (Test-Path $log) {
    Select-String -Path $log -Pattern ($patterns -join '|') | ForEach-Object { $_.Line }
    ''
    "captures logged: " + (Select-String -Path $log -Pattern 'capture \d+ action=' -AllMatches).Count
} else {
    "no log was written to $log; check $err"
}
