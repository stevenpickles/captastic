<#
.SYNOPSIS
    Samples a process's handle and memory counters to CSV, for soak runs.

.DESCRIPTION
    Milestone 5's soak criteria are about growth that does not level off, so the raw counters
    matter less than their trend across a long run. Four are sampled:

      Handles       kernel handles (files, events, threads, D3D objects)
      GdiObjects    GDI handles - device contexts, bitmaps, regions. The window-render path
                    creates these per capture, so a leak shows here first.
      UserObjects   USER handles - windows, hooks, cursors. The overlay and tray own these.
      WorkingSetMB / PrivateMB

    GDI and USER objects are not exposed by Get-Process and need GetGuiResources, which is why
    this script exists rather than a one-line loop.

.EXAMPLE
    .\sample-process-resources.ps1 -ProcessName captastic -IntervalSeconds 10 -OutputPath soak.csv
#>
[CmdletBinding()]
param(
    [string] $ProcessName = 'captastic',
    [int] $IntervalSeconds = 10,
    [Parameter(Mandatory)] [string] $OutputPath,
    [int] $MaxMinutes = 60
)

$ErrorActionPreference = 'Stop'

if (-not ('Captastic.GuiResources' -as [type])) {
    Add-Type -Namespace Captastic -Name GuiResources -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern uint GetGuiResources(System.IntPtr hProcess, uint uiFlags);
'@
}

'Timestamp,ElapsedSeconds,Handles,GdiObjects,UserObjects,WorkingSetMB,PrivateMB' |
    Set-Content -Path $OutputPath -Encoding UTF8

$started = Get-Date
$deadline = $started.AddMinutes($MaxMinutes)

while ((Get-Date) -lt $deadline) {
    $process = Get-Process -Name $ProcessName -ErrorAction SilentlyContinue |
        Sort-Object StartTime -Descending |
        Select-Object -First 1
    if (-not $process) {
        # The daemon exiting is the normal end of a soak, not an error.
        break
    }

    $gdi = [Captastic.GuiResources]::GetGuiResources($process.Handle, 0)
    $user = [Captastic.GuiResources]::GetGuiResources($process.Handle, 1)
    $elapsed = [int]((Get-Date) - $started).TotalSeconds

    $row = '{0},{1},{2},{3},{4},{5:N2},{6:N2}' -f (Get-Date -Format 'HH:mm:ss'),
        $elapsed,
        $process.HandleCount,
        $gdi,
        $user,
        ($process.WorkingSet64 / 1MB),
        ($process.PrivateMemorySize64 / 1MB)
    Add-Content -Path $OutputPath -Value $row

    Start-Sleep -Seconds $IntervalSeconds
}

Write-Output "sampling finished: $OutputPath"
