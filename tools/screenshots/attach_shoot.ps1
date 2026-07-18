# attach_shoot.ps1 [session] -- attach this terminal to an isolated named
# herdr session (default 'shoot'), with the parent's HERDR_* env cleared
# (herdr refuses "nested" launches otherwise). On exit, log the code and keep
# the window open briefly so failures are inspectable.
param([string]$Session = 'shoot')
Remove-Item Env:HERDR_* -ErrorAction SilentlyContinue
herdr session attach $Session
"exit=$LASTEXITCODE at $(Get-Date -Format o)" | Out-File -FilePath "$PSScriptRoot\attach.log" -Encoding utf8
Start-Sleep 300
