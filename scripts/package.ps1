$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
$cargoScript = Join-Path $PSScriptRoot 'cargo.ps1'
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)

function Get-WorkspaceVersion {
    $tomlPath = Join-Path $repo 'Cargo.toml'
    $text = Get-Content -Raw -LiteralPath $tomlPath
    $section = [regex]::Match(
        $text,
        '(?ms)^\[workspace\.package\]\s*(.*?)(?=^\[|\z)'
    )
    if (-not $section.Success) {
        throw "workspace.package section not found in $tomlPath"
    }
    $version = [regex]::Match($section.Groups[1].Value, '(?m)^version\s*=\s*"([^"]+)"')
    if (-not $version.Success) {
        throw "workspace version not found in $tomlPath"
    }
    $version.Groups[1].Value
}

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
        throw "refusing to operate outside $rootFull`: $full"
    }
    $full
}

function Remove-Safe {
    param(
        [Parameter(Mandatory = $true)][string] $Path,
        [Parameter(Mandatory = $true)][string] $Root
    )

    if (-not (Test-Path -LiteralPath $Path)) {
        return
    }
    $full = Assert-UnderDirectory -Path $Path -Root $Root
    Remove-Item -LiteralPath $full -Recurse -Force
}

$version = Get-WorkspaceVersion
$packageName = "hive-$version-windows-x86_64"
$distRoot = Join-Path $repo 'dist'
$stage = Join-Path $distRoot $packageName
$zipPath = Join-Path $distRoot "$packageName.zip"

New-Item -ItemType Directory -Force -Path $distRoot | Out-Null

Write-Host "==> build release binaries" -ForegroundColor Cyan
& $cargoScript build --release -p hive-cli -p hive-server
if ($LASTEXITCODE -ne 0) {
    throw "release build failed"
}

$cli = Join-Path $repo 'target\release\hive-cli.exe'
$server = Join-Path $repo 'target\release\hive-server.exe'
foreach ($binary in @($cli, $server)) {
    if (-not (Test-Path -LiteralPath $binary)) {
        throw "release binary not found: $binary"
    }
}

$reportedVersion = ((& $cli --version) -join "`n").Trim()
if ($reportedVersion -ne "Hive $version") {
    throw "version mismatch: Cargo.toml=$version, hive --version=$reportedVersion"
}

Remove-Safe -Path $stage -Root $distRoot
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'bin') | Out-Null
Copy-Item -LiteralPath $cli -Destination (Join-Path $stage 'bin\hive.exe')
Copy-Item -LiteralPath $server -Destination (Join-Path $stage 'bin\hive-server.exe')
Copy-Item -LiteralPath (Join-Path $repo 'packaging\hive.env.example') -Destination $stage
Copy-Item -LiteralPath (Join-Path $repo 'packaging\README.txt') -Destination $stage
Copy-Item -LiteralPath (Join-Path $repo 'packaging\start-hive.ps1') -Destination $stage
Copy-Item -LiteralPath (Join-Path $repo 'CHANGELOG.md') -Destination $stage

$commit = 'unknown'
try {
    $commit = ((& git -C $repo rev-parse --short HEAD) -join '').Trim()
} catch {
    $commit = 'unknown'
}
$builtAt = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
$release = Get-Content -Raw -LiteralPath (Join-Path $repo 'docs\RELEASE.md')
$release += @"

## 本包构建信息

- 版本: $version
- 构建 commit: $commit
- 构建时间(UTC): $builtAt
- 迁移目标版本: V6
- 平台: Windows x86_64
"@
[IO.File]::WriteAllText(
    (Join-Path $stage 'RELEASE.md'),
    $release,
    $utf8NoBom
)

$forbidden = Get-ChildItem -LiteralPath $stage -Recurse -Force | Where-Object {
    $_.Name -eq '.env' -or
    $_.Name -eq 'data' -or
    $_.Name -eq 'target' -or
    $_.Name -eq 'work'
}
if ($forbidden) {
    $names = ($forbidden | ForEach-Object { $_.FullName }) -join ', '
    throw "release stage contains forbidden paths: $names"
}

$sumLines = @()
foreach ($file in Get-ChildItem -LiteralPath $stage -Recurse -File | Sort-Object FullName) {
    $relative = [IO.Path]::GetRelativePath($stage, $file.FullName).Replace('\', '/')
    $hash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    $sumLines += "$hash  $relative"
}
[IO.File]::WriteAllLines(
    (Join-Path $stage 'SHA256SUMS.txt'),
    $sumLines,
    $utf8NoBom
)

Remove-Safe -Path $zipPath -Root $distRoot
Compress-Archive -LiteralPath $stage -DestinationPath $zipPath -CompressionLevel Optimal
if (-not (Test-Path -LiteralPath $zipPath)) {
    throw "zip was not created: $zipPath"
}

$zipHash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText(
    "$zipPath.sha256",
    "$zipHash  $packageName.zip`n",
    $utf8NoBom
)

Write-Host ''
Write-Host "Package: $zipPath"
Write-Host "Stage:   $stage"
Write-Host "SHA256:  $zipHash"
