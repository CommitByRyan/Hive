[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $TargetDir,
    [string] $From,
    [switch] $Force
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot

function Assert-UnderDirectory {
    param(
        [Parameter(Mandatory = $true)][string] $Path,
        [Parameter(Mandatory = $true)][string] $Root
    )

    $full = [IO.Path]::GetFullPath($Path)
    $rootFull = [IO.Path]::GetFullPath($Root)
    if (-not $rootFull.EndsWith([IO.Path]::DirectorySeparatorChar)) {
        $rootFull += [IO.Path]::DirectorySeparatorChar
    }
    if (-not $full.StartsWith($rootFull, [StringComparison]::OrdinalIgnoreCase)) {
        throw "refusing to read outside package root $rootFull`: $full"
    }
    $full
}

function Find-PackageRoot {
    param([Parameter(Mandatory = $true)][string] $SearchRoot)

    if (Test-Path -LiteralPath (Join-Path $SearchRoot 'bin\hive.exe')) {
        return [IO.Path]::GetFullPath($SearchRoot)
    }
    $candidates = @(
        Get-ChildItem -LiteralPath $SearchRoot -Directory -Force |
            Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'bin\hive.exe') }
    )
    if ($candidates.Count -eq 1) {
        return $candidates[0].FullName
    }
    if ($candidates.Count -eq 0) {
        return $null
    }
    throw "multiple package roots found under $SearchRoot"
}

function Test-PackageHashes {
    param([Parameter(Mandatory = $true)][string] $PackageRoot)

    $sumsPath = Join-Path $PackageRoot 'SHA256SUMS.txt'
    if (-not (Test-Path -LiteralPath $sumsPath)) {
        throw "SHA256SUMS.txt missing; refusing to install"
    }

    $verified = 0
    $listed = @{}
    foreach ($line in Get-Content -LiteralPath $sumsPath) {
        if ([string]::IsNullOrWhiteSpace($line)) {
            continue
        }
        $match = [regex]::Match($line, '^([0-9a-fA-F]{64})\s+(.+)$')
        if (-not $match.Success) {
            throw "invalid SHA256SUMS line: $line"
        }
        $expected = $match.Groups[1].Value.ToLowerInvariant()
        $relative = $match.Groups[2].Value.Replace('/', '\')
        $normalized = $relative.Replace('\', '/')
        $file = Assert-UnderDirectory -Path (Join-Path $PackageRoot $relative) -Root $PackageRoot
        if (-not (Test-Path -LiteralPath $file -PathType Leaf)) {
            throw "file listed in SHA256SUMS.txt is missing: $relative"
        }
        $actual = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $expected) {
            throw "SHA256 mismatch for $relative`nexpected: $expected`nactual:   $actual"
        }
        $listed[$normalized] = $true
        $verified += 1
    }

    foreach ($file in Get-ChildItem -LiteralPath $PackageRoot -Recurse -File -Force) {
        if ($file.Name -eq 'SHA256SUMS.txt') {
            continue
        }
        $relative = [IO.Path]::GetRelativePath($PackageRoot, $file.FullName).Replace('\', '/')
        if (-not $listed.ContainsKey($relative)) {
            throw "file is not listed in SHA256SUMS.txt: $relative"
        }
    }
    Write-Host "Verified $verified files from SHA256SUMS.txt"
}

function Copy-PackageRoot {
    param(
        [Parameter(Mandatory = $true)][string] $PackageRoot,
        [Parameter(Mandatory = $true)][string] $Target
    )

    foreach ($item in Get-ChildItem -LiteralPath $PackageRoot -Force) {
        if ($item.Name -eq 'data' -or
            $item.Name -eq 'sandbox' -or
            $item.Name -eq '.env' -or
            $item.Name -like '.env.*') {
            continue
        }
        Copy-Item -LiteralPath $item.FullName -Destination (Join-Path $Target $item.Name) -Recurse -Force
    }
}

if (-not $From) {
    $distRoot = Join-Path $repo 'dist'
    $candidates = @(
        Get-ChildItem -LiteralPath $distRoot -Filter 'hive-*-windows-x86_64.zip' -File -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending
    )
    if ($candidates.Count -eq 0) {
        throw "no release zip found under $distRoot; run scripts/package.ps1 first"
    }
    $From = $candidates[0].FullName
}

$source = [IO.Path]::GetFullPath($From)
if (-not (Test-Path -LiteralPath $source)) {
    throw "install source not found: $source"
}

$target = [IO.Path]::GetFullPath($TargetDir)
if (Test-Path -LiteralPath $target) {
    if (-not (Test-Path -LiteralPath $target -PathType Container)) {
        throw "target is not a directory: $target"
    }
    $entries = @(Get-ChildItem -LiteralPath $target -Force)
    if ($entries.Count -gt 0 -and -not $Force) {
        throw "target directory is not empty: $target. Re-run with -Force to overwrite package files; existing data/ and .env are preserved."
    }
} else {
    New-Item -ItemType Directory -Force -Path $target | Out-Null
}

$temporaryRoot = $null
try {
    if ((Test-Path -LiteralPath $source -PathType Leaf) -and
        [IO.Path]::GetExtension($source).Equals('.zip', [StringComparison]::OrdinalIgnoreCase)) {
        $temporaryRoot = Join-Path ([IO.Path]::GetTempPath()) ("hive-install-" + [guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Force -Path $temporaryRoot | Out-Null
        Expand-Archive -LiteralPath $source -DestinationPath $temporaryRoot -Force
        $packageRoot = Find-PackageRoot -SearchRoot $temporaryRoot
    } elseif (Test-Path -LiteralPath $source -PathType Container) {
        $packageRoot = Find-PackageRoot -SearchRoot $source
    } else {
        throw "install source must be a release zip or an extracted package directory: $source"
    }

    if (-not $packageRoot) {
        throw "package root containing bin\hive.exe was not found in $source"
    }

    Test-PackageHashes -PackageRoot $packageRoot
    Copy-PackageRoot -PackageRoot $packageRoot -Target $target
} finally {
    if ($temporaryRoot -and (Test-Path -LiteralPath $temporaryRoot)) {
        Remove-Item -LiteralPath $temporaryRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Write-Host ''
Write-Host "Installed Hive to $target"
Write-Host 'Next steps:'
Write-Host "  cd `"$target`""
Write-Host '  .\bin\hive.exe doctor'
Write-Host '  .\start-hive.ps1'
