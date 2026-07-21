# capture_exact.ps1 <exact-title> <out.png> -- capture a window by EXACT title
# using PrintWindow (PW_RENDERFULLCONTENT), so the shot is correct even when
# the window is occluded by other windows or on another monitor. Substring
# matching + CopyFromScreen once shipped a frame of the user's own terminal.
param([string]$Title, [string]$Out)
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public class Win32Print {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lParam);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }
    public static IntPtr Found = IntPtr.Zero;
    public static void Find(string title) {
        Found = IntPtr.Zero;
        EnumWindows((h, l) => {
            var sb = new StringBuilder(512); GetWindowText(h, sb, 512);
            if (IsWindowVisible(h) && sb.ToString() == title) { Found = h; return false; }
            return true;
        }, IntPtr.Zero);
    }
}
'@
[Win32Print]::Find($Title)
if ([Win32Print]::Found -eq [IntPtr]::Zero) { Write-Error "no window titled exactly '$Title'"; exit 1 }
$r = New-Object Win32Print+RECT
[Win32Print]::GetWindowRect([Win32Print]::Found, [ref]$r) | Out-Null
$w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
# 2 = PW_RENDERFULLCONTENT (needed for DWM/DirectComposition apps like WT)
$ok = [Win32Print]::PrintWindow([Win32Print]::Found, $hdc, 2)
$g.ReleaseHdc($hdc)
if (-not $ok) { Write-Error 'PrintWindow failed'; exit 1 }
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()
Write-Output "saved $Out ($w x $h)"
