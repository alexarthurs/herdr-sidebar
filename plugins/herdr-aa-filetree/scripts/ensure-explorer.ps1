# ensure-explorer.ps1 -- Windows [[events]] hook body: make sure the FOCUSED tab
# has an Explorer pane docked on the left, WITHOUT stealing the user's focus.
#
# Runs on tab.focused / workspace.focused, so it must be idempotent and quiet:
#   - the focused tab already has an Explorer pane -> do nothing
#   - otherwise open one exactly like open-explorer.ps1's OPEN branch, but
#     unfocused: no zoom cycle at the end, and if the split target WAS the
#     focused pane (the swap would leave focus on the explorer, since focus
#     follows the SLOT), hand focus back to the displaced pane afterwards.
#
# Because a tab gets its explorer at first focus -- while it usually still has a
# single full-height pane -- the explorer becomes a full-height left column, and
# later splits nest inside the right side and never shorten it.

$ErrorActionPreference = 'Continue'

$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)
[Console]::OutputEncoding = $Utf8NoBom
$OutputEncoding = $Utf8NoBom

$HerdrBin = if ($env:HERDR_BIN_PATH) { $env:HERDR_BIN_PATH } else { 'herdr' }

function Strip-Verbatim([string]$p) {
    if ($p -and $p.StartsWith('\\?\')) { return $p.Substring(4) }
    return $p
}
$PluginRoot = Strip-Verbatim (Split-Path -Parent $PSScriptRoot)
$Bin = Join-Path $PluginRoot 'target\release\herdr-aa-filetree.exe'
if (-not (Test-Path $Bin)) { exit 0 }

function Get-PaneId([string]$json) {
    return ([regex]'"pane_id":"([^"]+)"').Match($json).Groups[1].Value
}

# Focus events arrive in bursts (tab.focused + workspace.focused for one switch),
# and concurrent ensures each see "no Explorer yet" and open one — verified live:
# a single tab switch spawned four panes. Serialize with an atomic mkdir lock;
# losing the race just skips this ensure (the next focus event re-fires it).
$LockDir = Join-Path ([IO.Path]::GetTempPath()) 'herdr-aa-filetree-ensure.lock'
try {
    New-Item -ItemType Directory -Path $LockDir -ErrorAction Stop | Out-Null
} catch {
    $stale = $false
    try {
        $age = (Get-Date) - (Get-Item $LockDir -ErrorAction Stop).CreationTime
        $stale = $age.TotalSeconds -gt 30
    } catch {}
    if (-not $stale) { exit 0 }
    try {
        Remove-Item $LockDir -Recurse -Force -Confirm:$false -ErrorAction Stop
        New-Item -ItemType Directory -Path $LockDir -ErrorAction Stop | Out-Null
    } catch { exit 0 }
}

trap { try { Remove-Item $LockDir -Recurse -Force -Confirm:$false } catch {}; break }
function Release-Lock { try { Remove-Item $LockDir -Recurse -Force -Confirm:$false } catch {} }

# Snapshot AFTER acquiring the lock, so a just-finished ensure's rename is visible.
$PanesJson = (& $HerdrBin pane list | Out-String)

# Anything but OPEN means the focused tab already has its Explorer.
$Decision = ($PanesJson | & $Bin --launch-decision 2>$null)
if (-not $Decision) { Release-Lock; exit 0 }
if ($Decision.Trim() -ne 'OPEN') { Release-Lock; exit 0 }

$fp = ($PanesJson | & $Bin --focused-pane).Trim()
if (-not $fp) { Release-Lock; exit 0 }
$FocusedId, $FocusedCwd = $fp -split "`t", 2

$Target = $FocusedId
$Ratio = '0.25'
$plan = ((& $HerdrBin pane layout --pane $FocusedId | Out-String) | & $Bin --open-plan).Trim()
if ($plan) { $Target, $Ratio = $plan -split "`t", 2 }

$splitArgs = @('pane', 'split', $Target, '--direction', 'right', '--ratio', $Ratio, '--no-focus')
if ($FocusedCwd) { $splitArgs += @('--cwd', $FocusedCwd) }
$out = (& $HerdrBin @splitArgs | Out-String)
$np = Get-PaneId $out
if (-not $np) { Release-Lock; exit 0 }

& $HerdrBin pane swap --source-pane $np --target-pane $Target *> $null
& $HerdrBin pane run $np "& \`"$Bin\`""
& $HerdrBin pane rename $np Explorer *> $null

# The swap leaves focus with the LEFT SLOT: if we split the focused pane, the
# explorer now holds its slot and stole focus -- hand it back to the displaced
# pane, which sits directly to the explorer's right.
if ($Target -eq $FocusedId) {
    & $HerdrBin pane focus --direction right --pane $np *> $null
}
Release-Lock
exit 0
