' launch-ensure.vbs -- console-free bootstrap for the ensure sidecar.
'
' Hook/action commands can't reference the sidecar by relative path (Windows
' resolves a relative program against herdr's own directory), and any console
' intermediary (powershell/cmd) flashes a Windows Terminal window per focus
' event on Windows 11 -- even under CREATE_NO_WINDOW. wscript.exe is a
' PATH-resolvable GUI-subsystem host, and herdr runs hook commands with
' cwd = plugin root, so the relative SCRIPT argument resolves fine; the script
' then starts the GUI-subsystem sidecar by absolute path. No console anywhere.
'
' Arguments are passed through (the toggle action appends --toggle).

Set fso = CreateObject("Scripting.FileSystemObject")
root = fso.GetParentFolderName(fso.GetParentFolderName(WScript.ScriptFullName))
exe = fso.BuildPath(root, "target\release\herdr-aa-filetree-ensure.exe")
If Not fso.FileExists(exe) Then WScript.Quit 0

args = ""
For Each a In WScript.Arguments
    args = args & " " & a
Next

CreateObject("WScript.Shell").Run """" & exe & """" & args, 0, True
WScript.Quit 0
