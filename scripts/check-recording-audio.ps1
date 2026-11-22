#Requires -Version 7.0
<#
.SYNOPSIS
Checks that a recording's sound reader hears the computer's sound (ADR 0015).

.DESCRIPTION
Plays a one-second 440 Hz tone through the default output device during two tests, and passes
when both heard it: the loopback reader's device test, and a three-second MP4 recording with sound,
whose file must hold the tone. Needs an output device; the tone is
quiet, but turn the volume down first if you like: loopback reads the mix after Windows' volume,
and the test only asks for a sound that is not silence.

Exit code 0 when the tone was heard, 1 otherwise.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repositoryRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Push-Location $repositoryRoot
try {
    function Get-TestBinary([string]$Package, [string]$Target) {
        $build = cargo test -p $Package --lib --no-run --message-format=json 2>$null |
            ForEach-Object { try { $_ | ConvertFrom-Json } catch { $null } } |
            Where-Object { $_ -and $_.executable -and $_.target.name -eq $Target } |
            Select-Object -Last 1
        if (-not $build) {
            throw "The $Package tests did not build."
        }
        $build.executable
    }
    $tests = @(
        @{ Binary = Get-TestBinary 'captastic-windows' 'captastic_windows'; Filter = 'the_default_device_is_read' },
        @{ Binary = Get-TestBinary 'captastic-app' 'captastic_app'; Filter = 'an_mp4_with_sound_carries' }
    )

    # One second of a 440 Hz tone, 16-bit stereo at 48 kHz.
    $tone = Join-Path ([System.IO.Path]::GetTempPath()) "captastic-tone-$PID.wav"
    $rate = 48000
    $frames = $rate
    $stream = [System.IO.MemoryStream]::new()
    $writer = [System.IO.BinaryWriter]::new($stream)
    $writer.Write([Text.Encoding]::ASCII.GetBytes('RIFF')); $writer.Write([int](36 + $frames * 4))
    $writer.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt ')); $writer.Write([int]16)
    $writer.Write([int16]1); $writer.Write([int16]2); $writer.Write([int]$rate)
    $writer.Write([int]($rate * 4)); $writer.Write([int16]4); $writer.Write([int16]16)
    $writer.Write([Text.Encoding]::ASCII.GetBytes('data')); $writer.Write([int]($frames * 4))
    for ($i = 0; $i -lt $frames; $i++) {
        $value = [int16](12000 * [Math]::Sin(2 * [Math]::PI * 440 * $i / $rate))
        $writer.Write($value); $writer.Write($value)
    }
    $writer.Flush()
    [System.IO.File]::WriteAllBytes($tone, $stream.ToArray())

    $env:CAPTASTIC_EXPECT_SOUND = '1'
    $failed = 0
    foreach ($test in $tests) {
        $output = Join-Path ([System.IO.Path]::GetTempPath()) "captastic-audio-check-$PID.txt"
        $process = Start-Process -FilePath $test.Binary -PassThru -WindowStyle Hidden `
            -ArgumentList @($test.Filter, '--ignored', '--nocapture') `
            -RedirectStandardError $output -RedirectStandardOutput "$output.out"
        Start-Sleep -Milliseconds 800
        ([System.Media.SoundPlayer]::new($tone)).PlaySync()
        $process.WaitForExit()
        Write-Host "$($test.Filter):"
        Get-Content $output, "$output.out" | Select-String 'loudest|s of sound|test result|panicked' |
            ForEach-Object { Write-Host "  $($_.Line)" }
        if ($process.ExitCode -ne 0) { $failed++ }
        Remove-Item $output, "$output.out" -ErrorAction SilentlyContinue
    }
    Remove-Item $tone -ErrorAction SilentlyContinue
    exit $(if ($failed -eq 0) { 0 } else { 1 })
} finally {
    Remove-Item Env:\CAPTASTIC_EXPECT_SOUND -ErrorAction SilentlyContinue
    Pop-Location
}
