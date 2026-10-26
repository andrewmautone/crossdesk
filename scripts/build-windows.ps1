# Builds target\release\crossdesk.exe with the GNU toolchain.
# Needs rustup (stable-x86_64-pc-windows-gnu) and MinGW (winget install BrechtSanders.WinLibs.POSIX.UCRT).
$ErrorActionPreference = "Stop"
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
if (-not (Get-Command dlltool.exe -ErrorAction SilentlyContinue)) {
    $pkg = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages" -Directory -Filter "BrechtSanders.WinLibs*" -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($pkg) { $env:PATH = "$($pkg.FullName)\mingw64\bin;$env:PATH" }
    else { throw "MinGW not found. Run: winget install BrechtSanders.WinLibs.POSIX.UCRT" }
}
Set-Location (Split-Path $PSScriptRoot -Parent)
cargo build --release
Write-Host "Built: $(Resolve-Path target\release\crossdesk.exe)"
