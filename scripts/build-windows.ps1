# Builds target\release\crossdesk.exe with the GNU toolchain.
# Needs rustup (stable-x86_64-pc-windows-gnu) and MinGW (winget install BrechtSanders.WinLibs.POSIX.UCRT).
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
if (-not (Get-Command dlltool.exe -ErrorAction SilentlyContinue)) {
    $pkg = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages" -Directory -Filter "BrechtSanders.WinLibs*" -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($pkg) { $env:PATH = "$($pkg.FullName)\mingw64\bin;$env:PATH" }
    else { Write-Error "MinGW not found. Run: winget install BrechtSanders.WinLibs.POSIX.UCRT"; exit 1 }
}
Set-Location (Split-Path $PSScriptRoot -Parent)
# cargo reports progress on stderr; run it through cmd so PowerShell 5.1 does not treat that as an error.
cmd /c "cargo build --release 2>&1"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Host "Built: $(Resolve-Path target\release\crossdesk.exe)"
