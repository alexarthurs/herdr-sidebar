# resize_titled.ps1 <title-substring> <width> <height> -- resize the WT window
# whose title contains the substring (async SetWindowPos; WT-safe).
param([string]$TitlePart, [int]$W, [int]$H)
Add-Type @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public class Win32EnumR {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lParam);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int w, int h, uint flags);
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }
}
'@
$found = [IntPtr]::Zero
$cb = [Win32EnumR+EnumProc]{
    param($h, $l)
    if (-not [Win32EnumR]::IsWindowVisible($h)) { return $true }
    $cls = New-Object System.Text.StringBuilder 256
    [Win32EnumR]::GetClassName($h, $cls, 256) | Out-Null
    if ($cls.ToString() -ne 'CASCADIA_HOSTING_WINDOW_CLASS') { return $true }
    $txt = New-Object System.Text.StringBuilder 512
    [Win32EnumR]::GetWindowText($h, $txt, 512) | Out-Null
    if ($txt.ToString() -like "*$script:TitlePart*") { $script:found = $h; return $false }
    return $true
}
[Win32EnumR]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
if ($found -eq [IntPtr]::Zero) { Write-Error "no WT window with title matching '$TitlePart'"; exit 1 }
$rect = New-Object Win32EnumR+RECT
[Win32EnumR]::GetWindowRect($found, [ref]$rect) | Out-Null
Write-Output "was $($rect.Right - $rect.Left)x$($rect.Bottom - $rect.Top) at $($rect.Left),$($rect.Top)"
# SWP_NOZORDER(0x4) | SWP_NOACTIVATE(0x10) | SWP_ASYNCWINDOWPOS(0x4000)
[Win32EnumR]::SetWindowPos($found, [IntPtr]::Zero, $rect.Left, $rect.Top, $W, $H, 0x4014) | Out-Null
Write-Output "now ${W}x${H}"
