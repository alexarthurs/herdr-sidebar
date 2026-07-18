# resize_wt.ps1 <width> <height> -- resize the Windows Terminal window (keeps position).
param([int]$W, [int]$H)
Add-Type @'
using System;
using System.Runtime.InteropServices;
public class Win32Move {
    [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr hWnd, int x, int y, int w, int h, bool repaint);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }
}
'@
$proc = Get-Process WindowsTerminal -ErrorAction Stop | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
$rect = New-Object Win32Move+RECT
[Win32Move]::GetWindowRect($proc.MainWindowHandle, [ref]$rect) | Out-Null
Write-Output "was $($rect.Right - $rect.Left)x$($rect.Bottom - $rect.Top) at $($rect.Left),$($rect.Top)"
[Win32Move]::MoveWindow($proc.MainWindowHandle, $rect.Left, $rect.Top, $W, $H, $true) | Out-Null
Write-Output "now ${W}x${H}"
