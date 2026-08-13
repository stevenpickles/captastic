#Requires -Version 5.1

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repositoryRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) "captastic-hook-tests-$PID"

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) {
        throw $Message
    }
}

function Assert-ThrowsLike([scriptblock]$Action, [string]$Pattern) {
    try {
        & $Action
    } catch {
        if ($_.Exception.Message -notlike $Pattern) {
            throw "Expected error like '$Pattern', received: $($_.Exception.Message)"
        }
        return
    }
    throw "Expected action to fail with '$Pattern'."
}

function New-HookTestCli([string]$Path) {
    $source = @'
using System;
using System.IO;

public static class CaptasticHookTestCli
{
    public static int Main(string[] args)
    {
        string mode = Environment.GetEnvironmentVariable("CAPTASTIC_HOOK_TEST_MODE") ?? "not-running";
        string statePath = Environment.GetEnvironmentVariable("CAPTASTIC_HOOK_TEST_STATE") ?? "";
        if (args.Length == 0)
        {
            return 64;
        }
        if (args[0] == "status")
        {
            Console.Error.WriteLine("fixture status completed");
            if (mode == "status-failure")
            {
                return 7;
            }
            if (mode == "malformed")
            {
                Console.WriteLine("not-json");
                return 0;
            }
            if (mode == "unexpected")
            {
                Console.WriteLine("{\"status\":\"indeterminate\"}");
                return 0;
            }
            if (mode == "not-running" || (mode == "running-stops" && File.Exists(statePath)))
            {
                Console.WriteLine("{\"status\":\"not_running\"}");
                return 0;
            }
            Console.WriteLine("{\"status\":\"running\"}");
            return 0;
        }
        if (args[0] == "stop")
        {
            Console.Error.WriteLine("fixture stop completed");
            if (mode == "stop-failure")
            {
                return 9;
            }
            if (mode == "running-stops" && statePath.Length > 0)
            {
                File.WriteAllText(statePath, "stopped");
            }
            return 0;
        }
        return 64;
    }
}
'@
    $sourcePath = [System.IO.Path]::ChangeExtension($Path, '.cs')
    [System.IO.File]::WriteAllText($sourcePath, $source, [System.Text.UTF8Encoding]::new($false))
    $compiler = Join-Path ([Environment]::GetFolderPath('Windows')) `
        'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
    if (-not (Test-Path -LiteralPath $compiler -PathType Leaf)) {
        throw "The .NET Framework C# compiler is required for hook tests: $compiler"
    }
    & $compiler /nologo /target:exe "/out:$Path" $sourcePath
    if ($LASTEXITCODE -ne 0) {
        throw "The hook test CLI failed to compile with exit code $LASTEXITCODE."
    }
}

try {
    $hookTools = Join-Path $testRoot 'tools'
    $hookApplication = Join-Path $hookTools 'captastic'
    New-Item -ItemType Directory -Path $hookApplication -Force | Out-Null
    $hookCli = Join-Path $hookApplication 'captastic.exe'
    New-HookTestCli $hookCli
    $hookScript = Join-Path $hookTools 'chocolateyBeforeModify.ps1'
    Copy-Item -LiteralPath (Join-Path $repositoryRoot 'packaging\chocolatey\tools\chocolateyBeforeModify.ps1') `
        -Destination $hookScript
    $env:CAPTASTIC_HOOK_TEST_STATE = Join-Path $testRoot 'hook-state.txt'

    $env:CAPTASTIC_HOOK_TEST_MODE = 'not-running'
    & $hookScript

    $env:CAPTASTIC_HOOK_TEST_MODE = 'running-stops'
    Remove-Item -LiteralPath $env:CAPTASTIC_HOOK_TEST_STATE -Force -ErrorAction SilentlyContinue
    & $hookScript
    Assert-True (Test-Path -LiteralPath $env:CAPTASTIC_HOOK_TEST_STATE -PathType Leaf) `
        'Chocolatey before-modify hook did not stop a running daemon.'

    $env:CAPTASTIC_HOOK_TEST_MODE = 'malformed'
    Assert-ThrowsLike { & $hookScript } '*status could not be read*'
    $env:CAPTASTIC_HOOK_TEST_MODE = 'status-failure'
    Assert-ThrowsLike { & $hookScript } '*status failed with exit code 7*'
    $env:CAPTASTIC_HOOK_TEST_MODE = 'unexpected'
    Assert-ThrowsLike { & $hookScript } "*unexpected status 'indeterminate'*"
    $env:CAPTASTIC_HOOK_TEST_MODE = 'stop-failure'
    Assert-ThrowsLike { & $hookScript } '*stop failed with exit code 9*'
    $env:CAPTASTIC_HOOK_TEST_MODE = 'always-running'
    Assert-ThrowsLike { & $hookScript } '*did not stop within five seconds*'

    Write-Host "Chocolatey hook tests passed under PowerShell $($PSVersionTable.PSVersion)."
} finally {
    Remove-Item Env:CAPTASTIC_HOOK_TEST_MODE -ErrorAction SilentlyContinue
    Remove-Item Env:CAPTASTIC_HOOK_TEST_STATE -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $testRoot -Recurse -Force -ErrorAction SilentlyContinue
}
