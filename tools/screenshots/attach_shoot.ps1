# attach_shoot.ps1 -- attach this terminal to the isolated 'shoot' herdr
# session, with the parent's HERDR_* env cleared (herdr refuses "nested"
# launches otherwise). On exit, log the code and keep the window open briefly
# so failures are inspectable.
Remove-Item Env:HERDR_* -ErrorAction SilentlyContinue
herdr session attach shoot
"exit=$LASTEXITCODE at $(Get-Date -Format o)" | Out-File -FilePath "$PSScriptRoot\attach.log" -Encoding utf8
Start-Sleep 300
