# Proves the daemon's wait-and-recover path against a machine that reports no attached displays.
#
# The blackout is injected (CAPTASTIC_TEST_NO_DISPLAYS_MS, debug builds only) because the real
# condition means unplugging the monitor this is being read on. Everything downstream of
# enumeration is the production path: the error kind, the daemon's decision to wait, the poll, the
# rebuild, and the captures that follow.
#
# Expected, in order:
#   1. the daemon starts and does NOT exit
#   2. it reports capture_engine=waiting_for_desktop and says what it is waiting for
#   3. triggers during the blackout are refused with the desktop message, not "recovering"
#   4. when the blackout lapses it builds the engine by itself and captures succeed
param(
    [string] $Repo = 'C:\Users\Steven\work\captastic',
    [string] $OutDir,
    [int] $BlackoutMs = 9000,
    [string] $UserExe = 'C:\ProgramData\chocolatey\lib\captastic\tools\captastic\captastic.exe'
)

$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $OutDir | Out-Null
$log = Join-Path $OutDir 'daemon.log'
$err = Join-Path $OutDir 'daemon.err'

$userDaemon = Get-Process captastic -ErrorAction SilentlyContinue
if ($userDaemon) {
    & $UserExe stop 2>&1 | Out-Null
    Start-Sleep -Seconds 2
}

$env:CAPTASTIC_TEST_NO_DISPLAYS_MS = "$BlackoutMs"
$exe = Join-Path $Repo 'target\debug\captastic.exe'
# Clipboard and selection off: this is about the capture engine's lifecycle, and a soak that
# quietly replaces the user's clipboard is how their clipboard got destroyed twice before.
$args = @(
    'daemon','--backend','dxgi','--clipboard','false','--selection','false',
    '--self-trigger','--self-trigger-interval-ms','1500','--max-captures','12'
)
$process = Start-Process -FilePath $exe -ArgumentList $args -RedirectStandardOutput $log `
    -RedirectStandardError $err -PassThru -WindowStyle Hidden -Environment @{ CAPTASTIC_TEST_NO_DISPLAYS_MS = "$BlackoutMs" }

$deadline = (Get-Date).AddSeconds(60)
while (-not $process.HasExited -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
if (-not $process.HasExited) {
    & $exe stop 2>&1 | Out-Null
    Start-Sleep -Seconds 3
    if (-not $process.HasExited) { Stop-Process -Id $process.Id -Force }
}
Remove-Item Env:\CAPTASTIC_TEST_NO_DISPLAYS_MS -ErrorAction SilentlyContinue

if ($userDaemon) {
    Start-Process -FilePath $UserExe -WindowStyle Hidden
    Start-Sleep -Seconds 4
}
$back = Get-Process captastic -ErrorAction SilentlyContinue
"user's daemon: " + $(if ($back) { "running (pid $($back.Id))" } else { 'NOT RUNNING' })
"exit code: $($process.ExitCode)"
