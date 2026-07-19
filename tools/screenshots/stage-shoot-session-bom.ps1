# stage-shoot-session.ps1 [-Session shoot] [-WithGrid] -- stand up the SHARED
# screenshot backdrop for the herdr-sidebar* plugin repos in an isolated named
# herdr session (never touches the real session). Idempotent: safe to re-run.
#
# Roster (agreed between the herdr-aa-notes and herdr-sidebar agents; also
# recorded in both repos' CLAUDE.md "README screenshots" sections):
#   spaces:  acme-app [main, 1 ahead]  acme-api [main]  acme-web [dev]
#            billing-service [main]
#   agents:  fake background rows via socket pane.report_agent —
#            flaky-tests (codex, working, acme-api), reviewer (claude, idle,
#            acme-web), migrations (codex, working, billing-service)
#   -WithGrid additionally stages acme-app t1's REAL 2x2 grid: claude
#   'auth-refactor', codex 'checkout-tests', codex 'api-docs' (+composer
#   text), claude 'rate-limiter' (+composer text). Columns MUST stay <=63 —
#   Claude's banner includes the user's EMAIL at 74+ cols.
#
# After staging, a WT window titled "herdr-<session>" attaches for capture:
#   capture_titled.ps1 "herdr-<session>" out.png ; crop.ps1 raw out 8 48 1744 940
param([string]$Session = 'shoot', [switch]$WithGrid)
$ErrorActionPreference = 'Stop'
$Tools = $PSScriptRoot

# 1. Server: start headless if this session is not already running.
$sessions = (herdr session list --json | ConvertFrom-Json).sessions
$mine = $sessions | Where-Object { $_.name -eq $Session }
if (-not $mine -or -not $mine.running) {
    Start-Process herdr -ArgumentList '--session', $Session, 'server' -WindowStyle Hidden
    Start-Sleep -Seconds 4
    $mine = (herdr session list --json | ConvertFrom-Json).sessions | Where-Object { $_.name -eq $Session }
    if (-not $mine.running) { throw "session '$Session' failed to start" }
}
$env:HERDR_SOCKET_PATH = $mine.socket_path
Write-Output "session '$Session' at $($mine.socket_path)"

function Invoke-Rpc([string]$Method, [string]$ParamsJson) {
    $ParamsJson | python "$Tools\herdr_rpc.py" $env:HERDR_SOCKET_PATH $Method
}

# 2. Demo repos: acme-app via setup_demo.sh; backdrop cwds as git-init'd dirs
#    (git repo => branch sublabel renders in the spaces list).
if (-not (Test-Path "$env:USERPROFILE\Projects\acme-app\.git")) {
    Push-Location $Tools; bash setup_demo.sh | Out-Null; Pop-Location
}
$stage = "$env:LOCALAPPDATA\herdr-shoot-stage"
foreach ($spec in @(@('acme-api', 'main'), @('acme-web', 'dev'), @('billing-service', 'main'))) {
    $dir = Join-Path $stage $spec[0]
    if (-not (Test-Path "$dir\.git")) {
        New-Item -ItemType Directory -Force $dir | Out-Null
        Push-Location $dir
        git init -q -b $spec[1] .
        "# $($spec[0])" | Out-File README.md -Encoding utf8
        git add -A; git commit -qm init
        Pop-Location
    }
}

# 3. Workspaces (creation order = spaces-list order), skipping existing labels.
$existing = @((herdr workspace list | ConvertFrom-Json).result.workspaces | ForEach-Object { $_.label })
$wsIds = @{}
foreach ($spec in @(
        @('acme-app', "$env:USERPROFILE\Projects\acme-app"),
        @('acme-api', (Join-Path $stage 'acme-api')),
        @('acme-web', (Join-Path $stage 'acme-web')),
        @('billing-service', (Join-Path $stage 'billing-service')))) {
    if ($existing -contains $spec[0]) {
        $wsIds[$spec[0]] = ((herdr workspace list | ConvertFrom-Json).result.workspaces | Where-Object { $_.label -eq $spec[0] } | Select-Object -First 1).workspace_id
    } else {
        $w = (herdr workspace create --label $spec[0] --cwd $spec[1] --no-focus | ConvertFrom-Json).result.workspace
        $wsIds[$spec[0]] = $w.workspace_id
    }
}
Start-Sleep -Milliseconds 1000
Write-Output ("workspaces: " + (($wsIds.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join ' '))

# 4. Fake background agent rows (no CLI spawned; persists over detection).
foreach ($f in @(
        @{ws = 'acme-api'; label = 'flaky-tests'; agent = 'codex'; state = 'working' },
        @{ws = 'acme-web'; label = 'reviewer'; agent = 'claude'; state = 'idle' },
        @{ws = 'billing-service'; label = 'migrations'; agent = 'codex'; state = 'working' })) {
    $p = ((herdr pane list --workspace $wsIds[$f.ws] | ConvertFrom-Json).result.panes | Select-Object -First 1)
    if ($p.label -ne $f.label) { herdr pane rename $p.pane_id $f.label | Out-Null }
    if ($p.agent -ne $f.agent) {
        Invoke-Rpc 'pane.report_agent' ('{"pane_id":"' + $p.pane_id + '","source":"shoot-stage","agent":"' + $f.agent + '","state":"' + $f.state + '"}') | Out-Null
    }
}

# 5. Optional 2x2 agent grid in acme-app t1 (skips if a labeled pane exists).
if ($WithGrid) {
    $app = $wsIds['acme-app']
    $panes = (herdr pane list --workspace $app | ConvertFrom-Json).result.panes
    if (-not ($panes | Where-Object { $_.label -eq 'auth-refactor' })) {
        $tl = ($panes | Select-Object -First 1).pane_id
        $tr = ((herdr pane split $tl --direction right --ratio 0.5 --no-focus | ConvertFrom-Json).result.pane.pane_id)
        $bl = ((herdr pane split $tl --direction down --ratio 0.5 --no-focus | ConvertFrom-Json).result.pane.pane_id)
        $br = ((herdr pane split $tr --direction down --ratio 0.5 --no-focus | ConvertFrom-Json).result.pane.pane_id)
        herdr pane rename $tl 'auth-refactor' | Out-Null
        herdr pane rename $tr 'checkout-tests' | Out-Null
        herdr pane rename $bl 'api-docs' | Out-Null
        herdr pane rename $br 'rate-limiter' | Out-Null
        herdr pane run $tl 'claude' | Out-Null
        herdr pane run $tr 'codex' | Out-Null
        herdr pane run $bl 'codex' | Out-Null
        herdr pane run $br 'claude' | Out-Null
        Start-Sleep -Seconds 9
        herdr pane send-text $bl 'Draft OpenAPI docs for the billing endpoints' | Out-Null
        herdr pane send-text $br 'Add a sliding-window rate limiter to the gateway' | Out-Null
        Write-Output "grid staged: $tl $tr $bl $br"
    } else { Write-Output 'grid already present' }
}

# 6. Display window for capture (attach clears inherited HERDR_* env).
herdr workspace focus $wsIds['acme-app'] | Out-Null
$title = "herdr-$Session"
$probe = & "$Tools\resize_titled.ps1" $title 1760 996 2>$null
if (-not $probe) {
    Start-Process wt.exe -ArgumentList '-w', 'new', 'nt', '--title', $title, 'powershell', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "$Tools\attach_shoot.ps1", $Session
    Start-Sleep -Seconds 8
    & "$Tools\resize_titled.ps1" $title 1760 996
} else { Write-Output $probe }
Write-Output "backdrop ready — capture with: capture_titled.ps1 '$title' <out.png>"
