# Builds the AllStore release artifacts:
#   dist\AllStore-<ver>-x64.msi               per-user installer
#   dist\AllStore-<ver>-windows-x64.zip       portable bundle (exe + LICENSE)
#   dist\AllStore-<ver>-windows-x64.exe       bare portable exe
#
# Requires: Rust toolchain, WiX v5 (`dotnet tool install -g wix --version 5.0.2`
# plus the UI/Util extensions). Run from anywhere: paths are repo-relative.

param(
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$ver = (Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"(.+)"').Matches[0].Groups[1].Value
Write-Host "AllStore v$ver" -ForegroundColor Cyan

if (-not $SkipBuild) {
    # cargo writes progress to stderr; PS 5.1 turns that into a terminating
    # NativeCommandError under $ErrorActionPreference = "Stop", so relax it here.
    $eap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    & cargo build --release
    $code = $LASTEXITCODE
    $ErrorActionPreference = $eap
    if ($code -ne 0) { throw "cargo build failed" }
}

$exe = Join-Path $root "target\release\allstore.exe"
if (-not (Test-Path $exe)) { throw "missing $exe" }

New-Item -ItemType Directory -Force -Path (Join-Path $root "dist") | Out-Null

# --- portable exe + zip -----------------------------------------------------
$portableExe = Join-Path $root "dist\AllStore-$ver-windows-x64.exe"
Copy-Item $exe $portableExe -Force

$stage = Join-Path $env:TEMP "allstore-portable-$ver"
if (Test-Path $stage) { [System.IO.Directory]::Delete($stage, $true) }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
Copy-Item $exe (Join-Path $stage "AllStore.exe") -Force
Copy-Item (Join-Path $root "LICENSE") $stage -Force
Copy-Item (Join-Path $root "installer\PORTABLE.txt") $stage -Force

$zip = Join-Path $root "dist\AllStore-$ver-windows-x64.zip"
if (Test-Path $zip) { [System.IO.File]::Delete($zip) }
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip -CompressionLevel Optimal
[System.IO.Directory]::Delete($stage, $true)

# --- MSI --------------------------------------------------------------------
$msi = Join-Path $root "dist\AllStore-$ver-x64.msi"
if (Test-Path $msi) { [System.IO.File]::Delete($msi) }

wix build `
    -arch x64 `
    -d "ProductVersion=$ver" `
    -d "SourceExe=$exe" `
    -d "IconFile=$(Join-Path $root 'assets\allstore.ico')" `
    -d "LicenseRtf=$(Join-Path $root 'installer\license.rtf')" `
    -ext WixToolset.UI.wixext `
    -ext WixToolset.Util.wixext `
    -out $msi `
    (Join-Path $root "installer\allstore.wxs")
if ($LASTEXITCODE -ne 0) { throw "wix build failed" }

# The .wixpdb is a debug symbol dump, not a deliverable.
$pdb = Join-Path $root "dist\AllStore-$ver-x64.wixpdb"
if (Test-Path $pdb) { [System.IO.File]::Delete($pdb) }

Get-ChildItem (Join-Path $root "dist\AllStore-$ver-*") |
    Select-Object Name, @{n = "MB"; e = { [math]::Round($_.Length / 1MB, 2) } } |
    Format-Table -AutoSize
