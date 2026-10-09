$ErrorActionPreference = 'Stop'
$Root = Join-Path ([IO.Path]::GetTempPath()) ("herdr-sidebar-test-" + [Guid]::NewGuid().ToString('N'))
$Fixture = Join-Path $Root 'fixture'
$Out = Join-Path $Root 'out'
New-Item -ItemType Directory -Path $Fixture -Force | Out-Null
try {
    $Version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
    $Triple = 'x86_64-pc-windows-msvc'
    $Assets = @("herdr-sidebar-$Triple.exe", "herdr-sidebar-ensure-$Triple.exe")
    foreach ($Asset in $Assets) {
        [IO.File]::WriteAllText((Join-Path $Fixture $Asset), "verified-prebuilt-$Version-$Asset", (New-Object Text.UTF8Encoding($false)))
    }
    $Lines = foreach ($Asset in $Assets) {
        "{0}  {1}" -f (Get-FileHash -LiteralPath (Join-Path $Fixture $Asset) -Algorithm SHA256).Hash.ToLowerInvariant(), $Asset
    }
    [IO.File]::WriteAllLines((Join-Path $Fixture 'SHA256SUMS'), $Lines, (New-Object Text.UTF8Encoding($false)))

    $env:HS_FETCH_DIR = $Fixture
    $env:HS_TEST_MODE = '1'
    $env:HS_OUT_DIR = $Out
    $env:HS_CARGO_TOML = (Resolve-Path Cargo.toml).Path
    $env:HS_ARCH = 'AMD64'
    & powershell -NoProfile -ExecutionPolicy Bypass -File scripts/fetch-or-build.ps1
    if ($LASTEXITCODE -ne 0) { throw 'prebuilt install test failed' }
    if ((Get-FileHash (Join-Path $Out 'herdr-sidebar.exe')).Hash -ne (Get-FileHash (Join-Path $Fixture $Assets[0])).Hash) { throw 'main binary mismatch' }
    if ((Get-FileHash (Join-Path $Out 'herdr-sidebar-ensure.exe')).Hash -ne (Get-FileHash (Join-Path $Fixture $Assets[1])).Hash) { throw 'ensure binary mismatch' }

    # Issue #96: herdr launched from PowerShell 7 hands this 5.1 child a
    # PSModulePath whose first entries are CoreCLR modules that claim, but
    # cannot provide, cmdlets such as Get-FileHash. Simulate that with a decoy
    # Microsoft.PowerShell.Utility module; the installer must still verify.
    $Decoy = Join-Path $Root 'psmodules'
    $DecoyModule = Join-Path $Decoy 'Microsoft.PowerShell.Utility\99.0.0.0'
    New-Item -ItemType Directory -Path $DecoyModule -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $DecoyModule 'Microsoft.PowerShell.Utility.psm1'), '# claims commands it cannot provide')
    [IO.File]::WriteAllText((Join-Path $DecoyModule 'Microsoft.PowerShell.Utility.psd1'), "@{ ModuleVersion = '99.0.0.0'; RootModule = 'Microsoft.PowerShell.Utility.psm1'; FunctionsToExport = @('Get-FileHash', 'Select-String', 'Invoke-WebRequest'); CmdletsToExport = @(); AliasesToExport = @() }")
    Remove-Item -LiteralPath $Out -Recurse -Force
    $SavedModulePath = $env:PSModulePath
    try {
        $env:PSModulePath = "$Decoy;" + (Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\Modules')
        & powershell -NoProfile -ExecutionPolicy Bypass -File scripts/fetch-or-build.ps1
        $PollutedExit = $LASTEXITCODE
    } finally {
        $env:PSModulePath = $SavedModulePath
    }
    if ($PollutedExit -ne 0) { throw 'prebuilt install failed under a PowerShell 7 module path' }
    if ((Get-FileHash (Join-Path $Out 'herdr-sidebar.exe')).Hash -ne (Get-FileHash (Join-Path $Fixture $Assets[0])).Hash) { throw 'main binary mismatch under a PowerShell 7 module path' }

    Remove-Item -LiteralPath (Join-Path $Fixture $Assets[1]) -Force
    $Marker = Join-Path $Root 'fallback.txt'
    $Stub = Join-Path $Root 'cargo.cmd'
    [IO.File]::WriteAllText($Stub, "@echo off`r`necho %* > `"$Marker`"`r`n", (New-Object Text.ASCIIEncoding))
    $env:HS_CARGO = $Stub
    & powershell -NoProfile -ExecutionPolicy Bypass -File scripts/fetch-or-build.ps1
    if ($LASTEXITCODE -ne 0) { throw 'source fallback test failed' }
    if ((Get-Content -LiteralPath $Marker -Raw).Trim() -ne 'build --release') { throw 'cargo fallback was not invoked' }

    [IO.File]::WriteAllText((Join-Path $Fixture $Assets[1]), "verified-prebuilt-$Version-$($Assets[1])", (New-Object Text.UTF8Encoding($false)))
    Add-Content -LiteralPath (Join-Path $Fixture $Assets[0]) -Value 'tampered'
    Remove-Item -LiteralPath $Marker -Force
    & powershell -NoProfile -ExecutionPolicy Bypass -File scripts/fetch-or-build.ps1
    if ($LASTEXITCODE -ne 0) { throw 'checksum fallback test failed' }
    if ((Get-Content -LiteralPath $Marker -Raw).Trim() -ne 'build --release') { throw 'checksum mismatch did not invoke cargo fallback' }
} finally {
    Remove-Item Env:HS_FETCH_DIR,Env:HS_TEST_MODE,Env:HS_OUT_DIR,Env:HS_CARGO_TOML,Env:HS_ARCH,Env:HS_CARGO -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $Root -Recurse -Force -ErrorAction SilentlyContinue
}
