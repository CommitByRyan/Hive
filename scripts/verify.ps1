$ErrorActionPreference = 'Stop'
$cargoScript = Join-Path $PSScriptRoot 'cargo.ps1'
$failed = @()
$skipClippy = $args -contains '-SkipClippy'

function Invoke-Step {
    param([string] $Name, [string[]] $Arguments)

    Write-Host "==> $Name" -ForegroundColor Cyan
    & $cargoScript @Arguments
    if ($LASTEXITCODE -ne 0) {
        $script:failed += $Name
        Write-Host "FAILED: $Name" -ForegroundColor Red
    }
}

Invoke-Step -Name 'fmt' -Arguments @('fmt', '--all', '--check')
if (-not $skipClippy) {
    Invoke-Step -Name 'clippy' -Arguments @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings')
}
Invoke-Step -Name 'test' -Arguments @('test', '--workspace')

Write-Host ''
if ($failed.Count -gt 0) {
    Write-Host ("FAILED steps: " + ($failed -join ', ')) -ForegroundColor Red
    exit 1
}
Write-Host 'All checks passed.' -ForegroundColor Green
