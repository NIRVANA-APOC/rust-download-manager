# Capture a screenshot of the RDM GUI window (not the whole desktop).
# Uses the DWM-composited screen rectangle of the top-level window, which is
# reliable for GPU-rendered windows (PrintWindow often returns black for
# DirectComposition swap chains).
[CmdletBinding()]
param(
    [string]$ProcessName = "rdm-gui",
    [string]$OutFile = "docs/evidence/rdm-gui.png",
    [int]$WaitSeconds = 3
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms

Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public static class Win32Window {
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc proc, IntPtr lParam);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int cmd);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    public static List<IntPtr> WindowsOf(uint targetPid) {
        var list = new List<IntPtr>();
        EnumWindows((h, l) => {
            uint pid; GetWindowThreadProcessId(h, out pid);
            if (pid == targetPid && IsWindowVisible(h)) list.Add(h);
            return true;
        }, IntPtr.Zero);
        return list;
    }
}
"@

$proc = Get-Process -Name $ProcessName -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $proc) { throw "process '$ProcessName' is not running; start it first" }

$hwnd = [IntPtr]::Zero
for ($i = 0; $i -lt 40; $i++) {
    $windows = [Win32Window]::WindowsOf([uint32]$proc.Id) | Where-Object {
        $r = New-Object Win32Window+RECT
        [Win32Window]::GetWindowRect($_, [ref]$r) | Out-Null
        ($r.Right - $r.Left) -gt 300 -and ($r.Bottom - $r.Top) -gt 200
    }
    if ($windows) { $hwnd = $windows | Select-Object -First 1; break }
    Start-Sleep -Milliseconds 250
}
if ($hwnd -eq [IntPtr]::Zero) { throw "no visible top-level window found for pid $($proc.Id)" }

[Win32Window]::ShowWindow($hwnd, 9) | Out-Null   # SW_RESTORE
[Win32Window]::SetForegroundWindow($hwnd) | Out-Null
Start-Sleep -Seconds $WaitSeconds

$rect = New-Object Win32Window+RECT
[Win32Window]::GetWindowRect($hwnd, [ref]$rect) | Out-Null
$w = $rect.Right - $rect.Left
$h = $rect.Bottom - $rect.Top
if ($w -le 0 -or $h -le 0) { throw "invalid window rect ${w}x${h}" }

$bmp = New-Object System.Drawing.Bitmap($w, $h)
$gfx = [System.Drawing.Graphics]::FromImage($bmp)
$gfx.CopyFromScreen($rect.Left, $rect.Top, 0, 0, (New-Object System.Drawing.Size($w, $h)))
$gfx.Dispose()

$dir = Split-Path -Parent $OutFile
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
$bmp.Save($OutFile, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Host "captured $($w)x$($h) window (hwnd=$hwnd pid=$($proc.Id)) -> $OutFile"
