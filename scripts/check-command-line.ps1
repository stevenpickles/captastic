#Requires -Version 7.0
<#
.SYNOPSIS
Checks captastic-cli end to end on this desktop: against a running daemon, and with none.

.DESCRIPTION
The desktop checklist for Milestone 10 (ADR 0014), as a script. It needs no hands, but it does use
this desktop while it runs, so run it when nobody is working at it:

- It opens three test windows of its own: red "A", blue "B", and a green "Cover" that sits over
  part of A. Every capture it takes is of these windows or of a rectangle inside B, written to a
  temporary folder, except one capture to the configured output folder, which it deletes again.
- It stops the Captastic that is running (through its control event, as the Chocolatey hook
  does), runs the daemon from -BinariesDirectory instead, and when it is done starts the
  executable that was running before again.
- One-shot captures raise their notification from a short-lived icon; the script clicks one of
  them through the icon's own window message, which opens an Explorer window on the temporary
  folder that the script then closes.

It does not check what only a person can: how a notification looks. A still capture refused
during a recording needs a recording, which needs the overlay; that one stays a manual step.

Exit code 0 when every check passed, 1 otherwise.

.PARAMETER BinariesDirectory
Where captastic.exe and captastic-cli.exe are. Defaults to target\debug, built first unless
-SkipBuild is given.
#>
[CmdletBinding()]
param(
    [string]$BinariesDirectory,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if (-not $BinariesDirectory) {
    $BinariesDirectory = Join-Path $repositoryRoot 'target\debug'
    if (-not $SkipBuild) {
        & cargo build -p captastic-app --bin captastic --bin captastic-cli
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed with exit code $LASTEXITCODE."
        }
    }
}
$BinariesDirectory = [System.IO.Path]::GetFullPath($BinariesDirectory)
$daemonExecutable = Join-Path $BinariesDirectory 'captastic.exe'
$cli = Join-Path $BinariesDirectory 'captastic-cli.exe'
foreach ($binary in @($daemonExecutable, $cli)) {
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw "Missing $binary."
    }
}

Add-Type -AssemblyName System.Drawing
Add-Type -Namespace CaptasticCheck -Name Native -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Unicode)]
public static extern IntPtr FindWindow(string className, string windowName);
[DllImport("user32.dll")]
public static extern bool PostMessage(IntPtr hwnd, uint message, IntPtr wparam, IntPtr lparam);
'@

$controlEventName = 'Local\CaptasticDaemonControl-v1'
$run = [Guid]::NewGuid().ToString('N').Substring(0, 8)
$workDirectory = Join-Path ([System.IO.Path]::GetTempPath()) "captastic-check-$run"
New-Item -ItemType Directory -Path $workDirectory | Out-Null
$results = [System.Collections.Generic.List[object]]::new()

function Add-Result([string]$Name, [bool]$Passed, [string]$Detail = '') {
    $results.Add([pscustomobject]@{ Check = $Name; Result = $(if ($Passed) { 'pass' } else { 'FAIL' }); Detail = $Detail })
    $mark = if ($Passed) { 'pass' } else { 'FAIL' }
    Write-Host ("[{0}] {1}{2}" -f $mark, $Name, $(if ($Detail) { " - $Detail" } else { '' }))
}

function Add-Skip([string]$Name, [string]$Detail) {
    $results.Add([pscustomobject]@{ Check = $Name; Result = 'skip'; Detail = $Detail })
    Write-Host "[skip] $Name - $Detail"
}

function Invoke-Cli([string[]]$Arguments) {
    $output = & $cli @Arguments 2>$null
    $code = $LASTEXITCODE
    $line = @($output | Where-Object { $_ -match '^\s*\{' }) | Select-Object -Last 1
    $json = if ($line) { $line | ConvertFrom-Json } else { $null }
    [pscustomobject]@{ Json = $json; Code = $code; Line = $line }
}

function Test-DaemonRunning {
    $controlEvent = $null
    if ([System.Threading.EventWaitHandle]::TryOpenExisting($controlEventName, [ref]$controlEvent)) {
        $controlEvent.Dispose()
        return $true
    }
    return $false
}

function Stop-Daemon {
    $controlEvent = $null
    if (-not [System.Threading.EventWaitHandle]::TryOpenExisting($controlEventName, [ref]$controlEvent)) {
        return
    }
    try { [void]$controlEvent.Set() } finally { $controlEvent.Dispose() }
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while ((Test-DaemonRunning) -and [DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Milliseconds 100
    }
    if (Test-DaemonRunning) {
        throw 'The running Captastic did not stop within ten seconds.'
    }
}

function Wait-Until([scriptblock]$Condition, [double]$Seconds) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    do {
        if (& $Condition) { return $true }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    return [bool](& $Condition)
}

# How a test window's pixels read in a capture: mostly its own colour, and none of the cover's.
function Measure-Colours([string]$Path) {
    $bitmap = [System.Drawing.Bitmap]::new($Path)
    try {
        $counts = @{ red = 0; blue = 0; green = 0; total = 0 }
        $stepX = [Math]::Max(1, [int]($bitmap.Width / 40))
        $stepY = [Math]::Max(1, [int]($bitmap.Height / 40))
        for ($y = 0; $y -lt $bitmap.Height; $y += $stepY) {
            for ($x = 0; $x -lt $bitmap.Width; $x += $stepX) {
                $pixel = $bitmap.GetPixel($x, $y)
                $counts.total++
                if ($pixel.R -gt 200 -and $pixel.G -lt 70 -and $pixel.B -lt 70) { $counts.red++ }
                elseif ($pixel.B -gt 200 -and $pixel.R -lt 70 -and $pixel.G -lt 70) { $counts.blue++ }
                elseif ($pixel.G -gt 200 -and $pixel.R -lt 90 -and $pixel.B -lt 90) { $counts.green++ }
            }
        }
        [pscustomobject]@{
            Width = $bitmap.Width
            Height = $bitmap.Height
            Red = $counts.red / $counts.total
            Blue = $counts.blue / $counts.total
            Green = $counts.green / $counts.total
        }
    } finally {
        $bitmap.Dispose()
    }
}

function Get-Announcers {
    @(Get-CimInstance Win32_Process -Filter "Name = 'captastic.exe'" |
        Where-Object { $_.CommandLine -like '*--announce-command-line-capture*' })
}

$helperScript = Join-Path $workDirectory 'windows.ps1'
$readyFile = Join-Path $workDirectory 'windows.ready'
Set-Content -LiteralPath $helperScript -Encoding utf8 -Value @'
param([string]$Run, [string]$Ready)
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
function New-CheckWindow([string]$Name, [System.Drawing.Color]$Colour, [int]$X, [int]$Y, [int]$Width, [int]$Height) {
    $form = [System.Windows.Forms.Form]::new()
    $form.Text = "Captastic check $Name $Run"
    $form.BackColor = $Colour
    $form.StartPosition = 'Manual'
    $form.Location = [System.Drawing.Point]::new($X, $Y)
    $form.Size = [System.Drawing.Size]::new($Width, $Height)
    $form.TopMost = $true
    $form.ShowInTaskbar = $true
    $form
}
$a = New-CheckWindow 'A' ([System.Drawing.Color]::FromArgb(255, 0, 0)) 100 100 600 400
$b = New-CheckWindow 'B' ([System.Drawing.Color]::FromArgb(0, 0, 255)) 800 100 600 400
$cover = New-CheckWindow 'Cover' ([System.Drawing.Color]::FromArgb(0, 255, 0)) 300 200 350 220
# Started with its console hidden, and Windows applies that to whichever window this process shows
# first. Showing each one a second time is honoured as asked.
foreach ($form in @($a, $b, $cover)) {
    $form.Show()
    $form.Hide()
    $form.Show()
}
$cover.Activate()
[System.IO.File]::WriteAllText($Ready, 'ready')
$started = [DateTime]::UtcNow
$timer = [System.Windows.Forms.Timer]::new()
$timer.Interval = 250
$timer.add_Tick({
    if ((Test-Path "$Ready.done") -or ([DateTime]::UtcNow - $started).TotalMinutes -gt 10) {
        [System.Windows.Forms.Application]::Exit()
    }
})
$timer.Start()
[System.Windows.Forms.Application]::Run()
'@

$previous = Get-Process captastic -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and $_.SessionId -eq (Get-Process -Id $PID).SessionId } |
    Select-Object -First 1
$previousPath = if ($previous) { $previous.Path } else { $null }
$helper = $null
$devDaemon = $null

try {
    Write-Host "Test windows and captures: $workDirectory"
    $helper = Start-Process pwsh -PassThru -WindowStyle Hidden -ArgumentList @(
        '-NoProfile', '-File', $helperScript, '-Run', $run, '-Ready', $readyFile
    )
    if (-not (Wait-Until { Test-Path $readyFile } 20)) {
        throw 'The test windows did not open.'
    }
    Start-Sleep -Milliseconds 800

    if (Test-DaemonRunning) {
        Write-Host "Stopping the running Captastic$(if ($previousPath) { " ($previousPath)" })."
        Stop-Daemon
    }
    $expectedVersion = ((& $cli --version) -split ' ')[-1]

    # ---- Without a daemon -------------------------------------------------------------------
    $status = Invoke-Cli @('status')
    Add-Result 'status without a daemon says it is not running' (
        $status.Code -eq 0 -and $status.Json.running -eq $false) $status.Line

    $listed = Invoke-Cli @('windows')
    $titleA = "Captastic check A $run"
    $titleB = "Captastic check B $run"
    $titleCover = "Captastic check Cover $run"
    $windowA = $listed.Json.windows | Where-Object title -eq $titleA
    $windowB = $listed.Json.windows | Where-Object title -eq $titleB
    $windowCover = $listed.Json.windows | Where-Object title -eq $titleCover
    Add-Result 'windows lists the three test windows' (
        $listed.Code -eq 0 -and $windowA -and $windowB -and $windowCover) "$(@($listed.Json.windows).Count) windows"
    if (-not ($windowA -and $windowB -and $windowCover)) {
        throw 'The test windows are not listed; nothing further can be checked.'
    }

    $oneShotA = Join-Path $workDirectory 'one-shot-a.png'
    $started = [DateTime]::UtcNow
    $capture = Invoke-Cli @('capture', '--window-title', $titleA, '--output', $oneShotA)
    $elapsed = ([DateTime]::UtcNow - $started).TotalSeconds
    $colours = if (Test-Path $oneShotA) { Measure-Colours $oneShotA } else { $null }
    Add-Result 'one-shot window capture' (
        $capture.Code -eq 0 -and $capture.Json.taken_by -eq 'one_shot' -and $colours -and
        $colours.Red -gt 0.6 -and $colours.Green -eq 0) (
        "exit $($capture.Code), red $([Math]::Round($colours.Red, 2)), green $([Math]::Round($colours.Green, 2))")
    Add-Result 'one-shot returns without waiting for the announcement' ($elapsed -lt 3) ("{0:N2} s" -f $elapsed)

    $announcerShown = Wait-Until { (Get-Announcers).Count -gt 0 } 3
    $announcerWindow = [IntPtr]::Zero
    [void](Wait-Until {
        # By its title too: PowerShell passes $null to a string parameter as an empty string.
        $script:announcerWindow = [CaptasticCheck.Native]::FindWindow('CaptasticAnnouncementWindow-v1', 'Captastic')
        $script:announcerWindow -ne [IntPtr]::Zero
    } 5)
    $announcerWindow = $script:announcerWindow
    Add-Result 'one-shot capture is announced from its own icon' ($announcerShown -and $announcerWindow -ne [IntPtr]::Zero)
    if ($announcerWindow -ne [IntPtr]::Zero) {
        # What the shell sends the icon's window when the notification is clicked.
        [void][CaptasticCheck.Native]::PostMessage($announcerWindow, 0x8001, [IntPtr]::Zero, [IntPtr]0x0405)
        $folder = Split-Path $workDirectory -Leaf
        $explorer = $null
        $revealed = Wait-Until {
            $script:explorer = (Invoke-Cli @('windows')).Json.windows |
                Where-Object { $_.process -eq 'explorer.exe' -and $_.title -like "*$folder*" } |
                Select-Object -First 1
            [bool]$script:explorer
        } 8
        Add-Result 'clicking the announcement shows the file in Explorer' $revealed
        Add-Result 'the icon goes soon after a click' (Wait-Until { (Get-Announcers).Count -eq 0 } 4)
        if ($script:explorer) {
            [void][CaptasticCheck.Native]::PostMessage([IntPtr][long]$script:explorer.handle, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
        }
    }

    $oneShotB = Join-Path $workDirectory 'one-shot-b.png'
    $capture = Invoke-Cli @('capture', '--window', [string]$windowB.handle, '--output', $oneShotB)
    $stillThere = Wait-Until { (Get-Announcers).Count -gt 0 } 3
    Start-Sleep -Seconds 5
    $afterFive = (Get-Announcers).Count -gt 0
    $gone = Wait-Until { (Get-Announcers).Count -eq 0 } 9
    Add-Result 'an unclicked announcement stays a while and then goes' (
        $capture.Code -eq 0 -and $stillThere -and $afterFive -and $gone)

    # ---- With the daemon from this build ----------------------------------------------------
    $devDaemon = Start-Process -FilePath $daemonExecutable -PassThru
    $running = Wait-Until { (Invoke-Cli @('status')).Json.running -eq $true } 20
    $status = Invoke-Cli @('status')
    Add-Result 'status with the daemon running' (
        $running -and $status.Json.captastic_version -eq $expectedVersion) $status.Line

    $displays = Invoke-Cli @('displays')
    Add-Result 'displays' ($displays.Code -eq 0 -and @($displays.Json.displays).Count -ge 1)

    $daemonA = Join-Path $workDirectory 'daemon-a.png'
    $capture = Invoke-Cli @('capture', '--window-title', $titleA, '--output', $daemonA)
    $colours = if (Test-Path $daemonA) { Measure-Colours $daemonA } else { $null }
    Add-Result 'window by title: the covered window, without its cover' (
        $capture.Code -eq 0 -and $capture.Json.taken_by -eq 'daemon' -and
        $capture.Json.captured.kind -eq 'window' -and $colours -and
        $colours.Red -gt 0.6 -and $colours.Green -eq 0) (
        "exit $($capture.Code), red $([Math]::Round($colours.Red, 2)), green $([Math]::Round($colours.Green, 2))")

    $daemonB = Join-Path $workDirectory 'daemon-b.jpg'
    $capture = Invoke-Cli @('capture', '--window', ('0x{0:X}' -f [long]$windowB.handle), '--output', $daemonB, '--format', 'jpeg')
    $colours = if (Test-Path $daemonB) { Measure-Colours $daemonB } else { $null }
    Add-Result 'window by hex handle, as JPEG' (
        $capture.Code -eq 0 -and $capture.Json.format -eq 'jpeg' -and $colours -and $colours.Blue -gt 0.6) "exit $($capture.Code)"

    $ambiguous = Invoke-Cli @('capture', '--window-title', "Captastic check")
    $matched = @($ambiguous.Json.matches | Where-Object { $_.title -like "*$run" }).Count
    Add-Result 'two or more matches are refused with the matches listed' (
        $ambiguous.Code -eq 2 -and $ambiguous.Json.reason -eq 'ambiguous_window' -and $matched -eq 3) "exit $($ambiguous.Code)"

    $process = Invoke-Cli @('capture', '--process', 'PWSH')
    Add-Result 'a process name matches its windows' (
        $process.Code -eq 2 -and $process.Json.reason -eq 'ambiguous_window' -and
        @($process.Json.matches | Where-Object title -eq $titleA).Count -eq 1) "exit $($process.Code)"

    $none = Invoke-Cli @('capture', '--window-title', "no window is called $run")
    Add-Result 'no match is refused' ($none.Code -eq 2 -and $none.Json.reason -eq 'no_window')

    # A rectangle in the middle of B: the desktop as shown, where B is uncovered.
    $rect = $windowB.rect
    $inner = '{0},{1},{2},{3}' -f ($rect.x + [int]($rect.width / 4)), ($rect.y + [int]($rect.height / 4)), [int]($rect.width / 2), [int]($rect.height / 2)
    $rectFile = Join-Path $workDirectory 'rect.png'
    $capture = Invoke-Cli @('capture', '--rect', $inner, '--output', $rectFile)
    $colours = if (Test-Path $rectFile) { Measure-Colours $rectFile } else { $null }
    Add-Result 'a rectangle is captured as shown' (
        $capture.Code -eq 0 -and $colours -and $colours.Blue -gt 0.9 -and
        $colours.Width -eq [int]($rect.width / 2)) "exit $($capture.Code)"

    $again = Invoke-Cli @('capture', '--rect', $inner, '--output', $rectFile)
    Add-Result 'an existing file is never written over' ($again.Code -eq 1 -and $again.Json.reason -eq 'write_failed')

    $relative = Join-Path $workDirectory 'relative'
    New-Item -ItemType Directory -Path $relative | Out-Null
    Push-Location $relative
    try {
        $capture = Invoke-Cli @('capture', '--rect', $inner, '--output', 'here.png')
    } finally {
        Pop-Location
    }
    Add-Result 'a relative output path is taken from the current folder' (
        $capture.Code -eq 0 -and (Test-Path (Join-Path $relative 'here.png')))

    $default = Invoke-Cli @('capture', '--rect', $inner)
    $defaultWritten = $default.Json.path -and (Test-Path -LiteralPath $default.Json.path)
    Add-Result 'without --output the capture goes to the output folder' ($default.Code -eq 0 -and $defaultWritten) "$($default.Json.path)"
    if ($defaultWritten) {
        Remove-Item -LiteralPath $default.Json.path -Force
    }

    foreach ($case in @(
        @{ Name = '--last-region'; Arguments = @('capture', '--last-region') },
        @{ Name = '--interactive'; Arguments = @('capture', '--interactive') },
        @{ Name = '--clipboard'; Arguments = @('capture', '--rect', $inner, '--clipboard') }
    )) {
        $answer = Invoke-Cli $case.Arguments
        Add-Result "$($case.Name) is refused as unsupported (until it is built)" (
            $answer.Code -eq 2 -and $answer.Json.reason -eq 'unsupported')
    }

    $usage = Invoke-Cli @('capture', '--rect', '1,2,3')
    Add-Result 'a wrong command line is a usage answer' ($usage.Code -eq 64 -and $usage.Json.status -eq 'usage')

    Add-Skip 'a capture during a recording is refused' 'needs a recording through the overlay; manual'

    Stop-Daemon
    $devDaemon = $null
    $status = Invoke-Cli @('status')
    Add-Result 'status after the daemon stops' ($status.Code -eq 0 -and $status.Json.running -eq $false)
} catch {
    Add-Result 'the check ran to the end' $false $_.Exception.Message
} finally {
    if ($devDaemon -and (Test-DaemonRunning)) {
        Stop-Daemon
    }
    Set-Content -LiteralPath "$readyFile.done" -Value 'done'
    if ($helper) {
        [void]$helper.WaitForExit(5000)
        if (-not $helper.HasExited) { $helper.Kill() }
    }
    if ($previousPath -and -not (Test-DaemonRunning)) {
        Write-Host "Starting the Captastic that was running before ($previousPath)."
        Start-Process -FilePath $previousPath
    }
}

$failed = @($results | Where-Object Result -eq 'FAIL').Count
$passed = @($results | Where-Object Result -eq 'pass').Count
$skipped = @($results | Where-Object Result -eq 'skip').Count
Write-Host ''
Write-Host "$passed passed, $failed failed, $skipped skipped. Captures are in $workDirectory"
exit $(if ($failed -eq 0) { 0 } else { 1 })
