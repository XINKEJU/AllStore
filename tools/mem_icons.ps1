# Measure the logo cache footprint: start an exe, scroll through a few pages of
# cards (each scroll loads more logos), then report working set and private
# bytes. Used to compare the DPI-scaled icon cap (0.14.10) against the fixed
# 128 px cap (0.14.9). Usage: mem_icons.ps1 <exe> [scrolls]
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [int]$Scrolls = 30
)

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public struct WRECT { public int Left, Top, Right, Bottom; }
public struct WPOINT { public int X, Y; }

public static class WU {
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, int d, UIntPtr e);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out WRECT r);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref WPOINT p);
}
'@

$p = Start-Process -FilePath $Exe -PassThru
Start-Sleep -Seconds 3

$hwnd = $p.MainWindowHandle
if ($hwnd -eq [IntPtr]::Zero) { Start-Sleep -Seconds 2; $p.Refresh(); $hwnd = $p.MainWindowHandle }
[WU]::SetForegroundWindow($hwnd) | Out-Null
Start-Sleep -Milliseconds 400

$r = New-Object WRECT
[WU]::GetClientRect($hwnd, [ref]$r) | Out-Null
$pt = New-Object WPOINT
$pt.X = [int](($r.Right - $r.Left) * 0.45)
$pt.Y = [int](($r.Bottom - $r.Top) * 0.55)
[WU]::ClientToScreen($hwnd, [ref]$pt) | Out-Null
[WU]::SetCursorPos($pt.X, $pt.Y) | Out-Null

for ($i = 0; $i -lt $Scrolls; $i++) {
    [WU]::mouse_event(0x0800, 0, 0, -120, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 220
}
Start-Sleep -Seconds 3

$p.Refresh()
[pscustomobject]@{
    Exe      = Split-Path $Exe -Leaf
    WS_MB    = [math]::Round($p.WorkingSet64 / 1MB, 1)
    Priv_MB  = [math]::Round($p.PrivateMemorySize64 / 1MB, 1)
    Client   = "$($r.Right - $r.Left)x$($r.Bottom - $r.Top)"
} | Format-List

Stop-Process -Id $p.Id -Force
