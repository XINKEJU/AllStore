# AllStore installer (per-user, no admin rights required)
#
# The MSI from the Releases page is the recommended way in; this script exists
# for portable-bundle users and source checkouts that want a Start-menu entry
# and an "Apps & features" row without running msiexec.
param(
    [string]$Exe = "",
    [switch]$NoShortcut
)

$ErrorActionPreference = "Stop"
$installDir = Join-Path $env:LOCALAPPDATA "AllStore"
$target = Join-Path $installDir "allstore.exe"

# Resolve the exe either from the distribution bundle (next to this script)
# or from a source checkout (target\release).
if ([string]::IsNullOrWhiteSpace($Exe)) {
    foreach ($c in @((Join-Path $PSScriptRoot "allstore.exe"),
                     (Join-Path $PSScriptRoot "target\release\allstore.exe"))) {
        if (Test-Path -LiteralPath $c) { $Exe = $c; break }
    }
    if ([string]::IsNullOrWhiteSpace($Exe)) {
        throw "allstore.exe not found next to install.ps1 (or under target\release)."
    }
}
if (-not (Test-Path -LiteralPath $Exe)) { throw "Build output not found: $Exe" }

# A running instance locks its exe. Stop only the one installed at $target,
# then retry the copy so an upgrade cannot fail on a sharing violation.
Get-Process allstore -ErrorAction SilentlyContinue | ForEach-Object {
    try { if ($_.Path -eq $target) { Stop-Process -Id $_.Id -Force } } catch {}
}
New-Item -ItemType Directory -Force -Path $installDir | Out-Null
for ($i = 1; $i -le 3; $i++) {
    try {
        Copy-Item -LiteralPath $Exe -Destination $target -Force -ErrorAction Stop
        break
    } catch {
        if ($i -eq 3) { throw "Cannot copy to $target : $_" }
        Start-Sleep -Milliseconds 400
    }
}

# --- migrate a pre-rename (MindosShop) install ------------------------------
# Remove the old shortcuts and uninstall row so "Apps & features" does not list
# the app twice. The old data dir is left alone: the new build copies its json
# state over on first launch.
$startMenu = Join-Path ([Environment]::GetFolderPath("StartMenu")) "Programs"
$desktop = [Environment]::GetFolderPath("Desktop")
foreach ($old in @((Join-Path $startMenu "MindosShop.lnk"),
                   (Join-Path $startMenu "MindosShop.url"),
                   (Join-Path $desktop "MindosShop.lnk"))) {
    if (Test-Path -LiteralPath $old) { Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue }
}
Remove-Item "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\MindosShop" -Recurse -Force -ErrorAction SilentlyContinue
Get-Process mindosshop -ErrorAction SilentlyContinue | ForEach-Object {
    try { Stop-Process -Id $_.Id -Force } catch {}
}

# --- shortcuts ---------------------------------------------------------------
$link = Join-Path $startMenu "AllStore.lnk"
if (-not $NoShortcut) {
    try {
        $shell = New-Object -ComObject WScript.Shell
        $sc = $shell.CreateShortcut($link)
        $sc.TargetPath = $target
        $sc.WorkingDirectory = $installDir
        $sc.Description = "AllStore — native Windows app store"
        $sc.IconLocation = "$target,0"
        $sc.Save()

        $dlink = Join-Path $desktop "AllStore.lnk"
        $ds = $shell.CreateShortcut($dlink)
        $ds.TargetPath = $target
        $ds.WorkingDirectory = $installDir
        $ds.Description = "AllStore — native Windows app store"
        $ds.IconLocation = "$target,0"
        $ds.Save()
    } catch {
        Write-Warning "COM shortcut failed, falling back to .url: $_"
        $urlLink = Join-Path $startMenu "AllStore.url"
        $fileUrl = "file:///" + ($target -replace "\\", "/")
        Set-Content -Path $urlLink -Value "[InternetShortcut]`r`nURL=$fileUrl`r`nIconFile=$target`r`nIconIndex=0" -Encoding ASCII
        $link = $urlLink
    }
}

# --- uninstall entry ---------------------------------------------------------
$reg = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\AllStore"
New-Item -Path $reg -Force | Out-Null
# The bundle may ship a VERSION file; source checkouts fall back to Cargo.toml.
$version = "0.11.0"
$verFile = Join-Path $PSScriptRoot "VERSION"
if (Test-Path -LiteralPath $verFile) {
    $v = (Get-Content -LiteralPath $verFile -Raw).Trim()
    if (-not [string]::IsNullOrWhiteSpace($v)) { $version = $v }
} else {
    $cargo = Join-Path $PSScriptRoot "Cargo.toml"
    if (Test-Path -LiteralPath $cargo) {
        $m = Select-String -Path $cargo -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
        if ($m) { $version = $m.Matches[0].Groups[1].Value }
    }
}
Set-ItemProperty -Path $reg -Name "DisplayName" -Value "AllStore"
Set-ItemProperty -Path $reg -Name "DisplayVersion" -Value $version
Set-ItemProperty -Path $reg -Name "Publisher" -Value "AllStore contributors"
Set-ItemProperty -Path $reg -Name "InstallLocation" -Value $installDir
Set-ItemProperty -Path $reg -Name "DisplayIcon" -Value $target
Set-ItemProperty -Path $reg -Name "URLInfoAbout" -Value "https://github.com/XINKEJU/AllStore"
Set-ItemProperty -Path $reg -Name "UninstallString" -Value "powershell.exe -ExecutionPolicy Bypass -File `"$installDir\uninstall.ps1`""
Set-ItemProperty -Path $reg -Name "NoModify" -Value 1 -Type DWord
Set-ItemProperty -Path $reg -Name "NoRepair" -Value 1 -Type DWord

$uninstall = @'
$dir = Split-Path -Parent $MyInvocation.MyCommand.Path
$programs = Join-Path ([Environment]::GetFolderPath("StartMenu")) "Programs"
foreach ($n in @("AllStore.lnk", "AllStore.url")) {
    $f = Join-Path $programs $n
    if (Test-Path $f) { Remove-Item $f -Force }
}
$d = Join-Path ([Environment]::GetFolderPath("Desktop")) "AllStore.lnk"
if (Test-Path $d) { Remove-Item $d -Force }
Remove-Item "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\AllStore" -Recurse -Force -ErrorAction SilentlyContinue
Get-Process allstore -ErrorAction SilentlyContinue | ForEach-Object {
    try { if ($_.Path -like "$dir\*") { Stop-Process -Id $_.Id -Force } } catch {}
}
Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
'@
Set-Content -Path (Join-Path $installDir "uninstall.ps1") -Value $uninstall -Encoding UTF8

Write-Host "Installed: $target"
Write-Host "Version: $version"
Write-Host "Size: $([math]::Round((Get-Item $target).Length / 1KB, 1)) KB"
Write-Host "Start menu: $link"
Write-Host "Uninstall: Settings > Apps > AllStore, or run $installDir\uninstall.ps1"
