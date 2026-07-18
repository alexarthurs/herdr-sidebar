# resize_wt_async.ps1 <width> <height> -- resize WT without blocking on its UI thread.
param([int]$W, [int]$H)
Add-Type @'
using System;
using System.Runtime.InteropServices;
public class Win32Async {
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int w, int h, uint flags);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }
}
'@
$proc = Get-Process WindowsTerminal -ErrorAction Stop | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
$rect = New-Object Win32Async+RECT
[Win32Async]::GetWindowRect($proc.MainWindowHandle, [ref]$rect) | Out-Null
Write-Output "was $($rect.Right - $rect.Left)x$($rect.Bottom - $rect.Top) at $($rect.Left),$($rect.Top)"
# SWP_NOZORDER(0x4) | SWP_NOACTIVATE(0x10) | SWP_ASYNCWINDOWPOS(0x4000)
[Win32Async]::SetWindowPos($proc.MainWindowHandle, [IntPtr]::Zero, $rect.Left, $rect.Top, $W, $H, 0x4014) | Out-Null
Start-Sleep -Milliseconds 800
[Win32Async]::GetWindowRect($proc.MainWindowHandle, [ref]$rect) | Out-Null
Write-Output "now $($rect.Right - $rect.Left)x$($rect.Bottom - $rect.Top)"
