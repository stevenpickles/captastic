#Requires -Version 7.0

[CmdletBinding()]
param()

& (Join-Path $PSScriptRoot 'test-packaging.ps1')
