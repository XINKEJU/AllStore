# MindosShop installer (per-user, no admin rights required)
param(
    [string]$Exe = "$PSScriptRoot\target\release\mindosshop.exe",
    [switch]$NoShortcut
)

$ErrorActionPreference = "Stop"
$installDir = Join-Path $env:LOCALAPPDATA "MindosShop"
$target = Join-Path $installDir "mindosshop.exe"

if (-not (Test-Path $Exe)) { throw "找不到构建产物: $Exe" }
New-Item -ItemType Directory -Force -Path $installDir | Out-Null
Copy-Item -LiteralPath $Exe -Destination $target -Force

# Start-menu shortcut
$startMenu = Join-Path ([Environment]::GetFolderPath("StartMenu")) "Programs"
$link = Join-Path $startMenu "MindosShop.lnk"
if (-not $NoShortcut) {
    try {
        $shell = New-Object -ComObject WScript.Shell
        $sc = $shell.CreateShortcut($link)
        $sc.TargetPath = $target
        $sc.WorkingDirectory = $installDir
        $sc.Description = "MindosShop - 纯净原生应用商店"
        $sc.IconLocation = "$target,0"
        $sc.Save()
    } catch {
        Write-Warning "COM 快捷方式创建失败，回退为 .url 快捷方式: $_"
        $urlLink = Join-Path $startMenu "MindosShop.url"
        $fileUrl = "file:///" + ($target -replace "\\", "/")
        Set-Content -Path $urlLink -Value "[InternetShortcut]`r`nURL=$fileUrl`r`nIconIndex=0" -Encoding ASCII
        $link = $urlLink
    }
}

# Uninstall entry
$reg = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\MindosShop"
New-Item -Path $reg -Force | Out-Null
Set-ItemProperty -Path $reg -Name "DisplayName" -Value "MindosShop"
Set-ItemProperty -Path $reg -Name "DisplayVersion" -Value "0.1.0"
Set-ItemProperty -Path $reg -Name "Publisher" -Value "MindosShop"
Set-ItemProperty -Path $reg -Name "InstallLocation" -Value $installDir
Set-ItemProperty -Path $reg -Name "DisplayIcon" -Value $target
Set-ItemProperty -Path $reg -Name "UninstallString" -Value "powershell.exe -ExecutionPolicy Bypass -File `"$installDir\uninstall.ps1`""
Set-ItemProperty -Path $reg -Name "NoModify" -Value 1 -Type DWord
Set-ItemProperty -Path $reg -Name "NoRepair" -Value 1 -Type DWord

$uninstall = @'
$dir = Split-Path -Parent $MyInvocation.MyCommand.Path
$lnk = Join-Path ([Environment]::GetFolderPath("StartMenu")) "Programs\MindosShop.lnk"
if (Test-Path $lnk) { Remove-Item $lnk -Force }
Remove-Item "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\MindosShop" -Recurse -Force -ErrorAction SilentlyContinue
Get-Process mindosshop -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
'@
Set-Content -Path (Join-Path $installDir "uninstall.ps1") -Value $uninstall -Encoding UTF8

Write-Host "已安装: $target"
Write-Host "体积: $([math]::Round((Get-Item $target).Length / 1KB, 1)) KB"
Write-Host "开始菜单: $link"
Write-Host "卸载: 设置 - 应用 - MindosShop（或运行 $installDir\uninstall.ps1）"
