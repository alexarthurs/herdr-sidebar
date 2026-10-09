# Install verified Windows release binaries, falling back to cargo on any miss.
# Compatible with the Windows PowerShell 5.1 host used by herdr actions.
$ErrorActionPreference = 'Stop'

# Herdr started from PowerShell 7 hands its PSModulePath to this Windows
# PowerShell 5.1 child. 5.1 then autoloads PS7's CoreCLR modules first and
# cmdlets such as Get-FileHash/Select-String silently vanish (issue #96). Keep
# only 5.1's own module directories, before any cmdlet triggers an autoload.
if ($PSVersionTable.PSVersion.Major -le 5) {
    $env:PSModulePath = @(
        [IO.Path]::Combine([Environment]::GetFolderPath('MyDocuments'), 'WindowsPowerShell', 'Modules'),
        [IO.Path]::Combine($env:ProgramFiles, 'WindowsPowerShell', 'Modules'),
        [IO.Path]::Combine($env:SystemRoot, 'System32', 'WindowsPowerShell', 'v1.0', 'Modules')
    ) -join ';'
}

$Repo = 'alexarthurs/herdr-sidebar'
$TestMode = $env:HS_TEST_MODE -eq '1'
function Remove-HsVerbatimPrefix([string]$Path) {
    if ($Path -and $Path.StartsWith('\\?\')) { return $Path.Substring(4) }
    return $Path
}
$NormalScriptRoot = Remove-HsVerbatimPrefix $PSScriptRoot
$RepoRoot = if ($TestMode -and $env:HS_REPO_ROOT) { $env:HS_REPO_ROOT } else { [IO.Path]::GetFullPath((Join-Path $NormalScriptRoot '..')) }
$CargoToml = if ($TestMode -and $env:HS_CARGO_TOML) { $env:HS_CARGO_TOML } else { Join-Path $RepoRoot 'Cargo.toml' }
$OutDir = if ($TestMode -and $env:HS_OUT_DIR) { $env:HS_OUT_DIR } else { Join-Path $RepoRoot 'target\release' }
$BaseUrl = if ($TestMode -and $env:HS_BASE_URL) { $env:HS_BASE_URL } else { "https://github.com/$Repo/releases/download" }
$script:TmpDir = $null

function Remove-HsTemp {
    if ($script:TmpDir -and (Test-Path -LiteralPath $script:TmpDir)) {
        Remove-Item -LiteralPath $script:TmpDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Build-HsFromSource {
    param([string]$Reason)
    [Console]::Error.WriteLine("herdr-sidebar: $Reason; building from source instead.")
    Remove-HsTemp
    $Cargo = if ($TestMode -and $env:HS_CARGO) { $env:HS_CARGO } else { 'cargo' }
    if (-not (Get-Command $Cargo -ErrorAction SilentlyContinue)) {
        [Console]::Error.WriteLine('herdr-sidebar needs a matching prebuilt release or Rust from https://rustup.rs')
        exit 1
    }
    Push-Location $RepoRoot
    try {
        & $Cargo build --release
        exit $LASTEXITCODE
    } finally {
        Pop-Location
    }
}

# .NET, not Get-FileHash: hashing must not depend on module autoloading.
function Get-HsSha256([string]$Path) {
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        $stream = [IO.File]::OpenRead($Path)
        try { $bytes = $sha.ComputeHash($stream) } finally { $stream.Dispose() }
    } finally { $sha.Dispose() }
    return [BitConverter]::ToString($bytes).Replace('-', '').ToLowerInvariant()
}

function Get-HsAsset {
    param([string]$Name, [string]$Destination)
    try {
        if ($TestMode -and $env:HS_FETCH_DIR) {
            Copy-Item -LiteralPath (Join-Path $env:HS_FETCH_DIR $Name) -Destination $Destination -Force
        } else {
            try {
                [Net.ServicePointManager]::SecurityProtocol =
                    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
            } catch {}
            Invoke-WebRequest -Uri "$BaseUrl/v$Version/$Name" -OutFile $Destination -UseBasicParsing
        }
        return $true
    } catch {
        return $false
    }
}

$Arch = if ($TestMode -and $env:HS_ARCH) { $env:HS_ARCH } elseif ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($Arch -ne 'AMD64') { Build-HsFromSource "no prebuilt binary for Windows/$Arch" }
$Triple = 'x86_64-pc-windows-msvc'

$VersionMatch = Select-String -Path $CargoToml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $VersionMatch) { Build-HsFromSource 'could not read the crate version' }
$Version = $VersionMatch.Matches[0].Groups[1].Value

$Assets = @(
    "herdr-sidebar-$Triple.exe",
    "herdr-sidebar-ensure-$Triple.exe"
)
$script:TmpDir = Join-Path ([IO.Path]::GetTempPath()) ("herdr-sidebar-fetch-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $script:TmpDir -Force | Out-Null
$Sums = Join-Path $script:TmpDir 'SHA256SUMS'
if (-not (Get-HsAsset 'SHA256SUMS' $Sums)) { Build-HsFromSource "checksums are unavailable for v$Version" }

foreach ($Asset in $Assets) {
    $Downloaded = Join-Path $script:TmpDir $Asset
    if (-not (Get-HsAsset $Asset $Downloaded)) { Build-HsFromSource "no prebuilt $Asset exists for v$Version" }
    $Escaped = [regex]::Escape($Asset)
    $Line = Get-Content -LiteralPath $Sums | Where-Object { $_ -match "^([0-9a-fA-F]{64}) [ *]${Escaped}$" } | Select-Object -First 1
    if (-not $Line) { Build-HsFromSource "the release does not list a checksum for $Asset" }
    $Expected = ([regex]::Match($Line, '^[0-9a-fA-F]{64}').Value).ToLowerInvariant()
    try {
        $Actual = Get-HsSha256 $Downloaded
    } catch {
        Build-HsFromSource "could not read the downloaded $Asset to verify it ($($_.Exception.Message))"
    }
    if ($Actual -ne $Expected) { Build-HsFromSource "checksum verification failed for $Asset" }
}

New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
Move-Item -LiteralPath (Join-Path $script:TmpDir $Assets[0]) -Destination (Join-Path $OutDir 'herdr-sidebar.exe') -Force
Move-Item -LiteralPath (Join-Path $script:TmpDir $Assets[1]) -Destination (Join-Path $OutDir 'herdr-sidebar-ensure.exe') -Force
Remove-HsTemp
[Console]::Out.WriteLine("herdr-sidebar: installed verified prebuilt v$Version ($Triple).")
exit 0
