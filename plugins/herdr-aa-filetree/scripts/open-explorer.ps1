# open-explorer.ps1 -- Windows launcher for the herdr-aa-filetree explorer pane.
#
# Idempotent "launch-or-focus, toggle on repeat", scoped to the current tab:
#   - no Explorer pane in the current tab      -> open one, DOCKED ON THE LEFT edge
#   - an Explorer pane exists but isn't focused -> focus it
#   - the focused pane IS the Explorer pane     -> close it (toggle off)
#
# Left dock: herdr's `pane split` only splits right/down, so we split the tab's
# LEFTMOST pane (the one touching the spaces/agents sidebar) to the right with a
# small left-slot ratio, then `pane swap` the new pane into that left slot.
# Verified against herdr 0.7.1: the split `--ratio` is the ORIGINAL pane's share,
# and after a swap the focus stays with the SLOT, not the pane.
#
# Windows caveats inherited from herdr-file-viewer (see its herdr-plugin.toml):
# herdr cannot spawn a relative [[panes]] command on Windows (ERROR_PATH_NOT_FOUND),
# so we spawn the binary BY ABSOLUTE PATH via `pane split` + `pane run`, and the
# pane-id / target / ratio decisions come from the binary's tested stdin modes
# (--launch-decision / --focused-pane / --open-plan), never from ad-hoc parsing.

$ErrorActionPreference = 'Continue'

# PowerShell 5.1 otherwise decodes herdr's UTF-8 JSON with the legacy console code
# page; non-ASCII pane titles or paths would corrupt the JSON.
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

if (-not (Test-Path $Bin)) {
    Write-Error "herdr-aa-filetree.exe not found at $Bin -- run 'cargo build --release' in the plugin directory first."
    exit 1
}

# Extract the first `pane_id` from a herdr CLI JSON reply.
function Get-PaneId([string]$json) {
    return ([regex]'"pane_id":"([^"]+)"').Match($json).Groups[1].Value
}

$PanesJson = (& $HerdrBin pane list | Out-String)

function Open-Pane {
    # Focused pane = where the user is working; its cwd roots the tree.
    $fp = ($PanesJson | & $Bin --focused-pane).Trim()
    if (-not $fp) {
        # No focused pane known: best-effort plain split beside the current pane.
        $out = (& $HerdrBin pane split --current --direction right --ratio 0.75 | Out-String)
        $np = Get-PaneId $out
        if ($np) { & $HerdrBin pane run $np "& \`"$Bin\`"" }
        exit 0
    }
    $FocusedId, $FocusedCwd = $fp -split "`t", 2

    # Left-dock plan: leftmost pane of the focused tab + the left-slot ratio.
    $Target = $FocusedId
    $Ratio = '0.25'
    $plan = ((& $HerdrBin pane layout --pane $FocusedId | Out-String) | & $Bin --open-plan).Trim()
    if ($plan) { $Target, $Ratio = $plan -split "`t", 2 }

    $splitArgs = @('pane', 'split', $Target, '--direction', 'right', '--ratio', $Ratio, '--no-focus')
    if ($FocusedCwd) { $splitArgs += @('--cwd', $FocusedCwd) }
    $out = (& $HerdrBin @splitArgs | Out-String)
    $np = Get-PaneId $out
    if (-not $np) { exit 1 }

    # Move the new pane into the left slot, then start the explorer in it.
    & $HerdrBin pane swap --source-pane $np --target-pane $Target *> $null
    # Absolute path via the PowerShell CALL OPERATOR: a bare path splits on spaces
    # in the install path, and the `\"` escaping survives PS 5.1's native-arg
    # quote-stripping so herdr receives the quotes intact (herdr-file-viewer GH #58).
    & $HerdrBin pane run $np "& \`"$Bin\`""
    & $HerdrBin pane rename $np Explorer *> $null
    # herdr has no focus-by-id; a zoom on/off cycle focuses deterministically.
    & $HerdrBin pane zoom $np --on *> $null
    & $HerdrBin pane zoom $np --off *> $null
    exit 0
}

$Decision = ($PanesJson | & $Bin --launch-decision 2>$null)
if ($LASTEXITCODE -ne 0 -or -not $Decision) { $Decision = 'OPEN' }
$Decision = $Decision.Trim()

if ($Decision -like 'FOCUS *') {
    $PaneId = $Decision.Substring(6)
    & $HerdrBin pane zoom $PaneId --on *> $null
    & $HerdrBin pane zoom $PaneId --off
    exit $LASTEXITCODE
} elseif ($Decision -like 'CLOSE *') {
    $PaneId = $Decision.Substring(6)
    & $HerdrBin pane close $PaneId
    exit $LASTEXITCODE
} else {
    Open-Pane
}
