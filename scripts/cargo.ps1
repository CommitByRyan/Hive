$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
$cargo = Join-Path $repo 'work\cargo\bin\cargo.exe'
$gcc = 'D:\software\CLion\CLion\CLion 2020.3.4\bin\mingw\bin\gcc.exe'
$mingwBin = Split-Path -Parent $gcc

if (-not (Test-Path -LiteralPath $cargo)) {
    throw "cargo not found at $cargo. The isolated toolchain under work/ is required."
}
if (-not (Test-Path -LiteralPath $gcc)) {
    throw "linker not found at $gcc."
}

$env:RUSTUP_HOME = Join-Path $repo 'work\rustup'
$env:CARGO_HOME = Join-Path $repo 'work\cargo'
$env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = $gcc
$env:PATH = "$mingwBin;$(Join-Path $repo 'work\cargo\bin');$env:PATH"

Push-Location $repo
try {
    & $cargo @args
    exit $LASTEXITCODE
}
finally {
    Pop-Location
}
