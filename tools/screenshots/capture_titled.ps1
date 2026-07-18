# capture_titled.ps1 <title-substring> <out.png> -- screenshot the Windows
# Terminal window whose title contains the given substring (works with
# multiple WT windows; WT is single-process so MainWindowHandle is useless).
param([string]$TitlePart, [string]$Out)
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public class Win32Enum {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lParam);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }
}
'@
$found = [IntPtr]::Zero
$cb = [Win32Enum+EnumProc]{
    param($h, $l)
    if (-not [Win32Enum]::IsWindowVisible($h)) { return $true }
    $cls = New-Object System.Text.StringBuilder 256
    [Win32Enum]::GetClassName($h, $cls, 256) | Out-Null
    if ($cls.ToString() -ne 'CASCADIA_HOSTING_WINDOW_CLASS') { return $true }
    $txt = New-Object System.Text.StringBuilder 512
    [Win32Enum]::GetWindowText($h, $txt, 512) | Out-Null
    if ($txt.ToString() -like "*$script:TitlePart*") { $script:found = $h; return $false }
    return $true
}
[Win32Enum]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
if ($found -eq [IntPtr]::Zero) { Write-Error "no WT window with title matching '$TitlePart'"; exit 1 }
[Win32Enum]::SetForegroundWindow($found) | Out-Null
Start-Sleep -Milliseconds 400
$rect = New-Object Win32Enum+RECT
[Win32Enum]::GetWindowRect($found, [ref]$rect) | Out-Null
$w = $rect.Right - $rect.Left
$h = $rect.Bottom - $rect.Top
$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($rect.Left, $rect.Top, 0, 0, (New-Object System.Drawing.Size($w, $h)))
$g.Dispose()
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Output "saved $Out ($w x $h)"
