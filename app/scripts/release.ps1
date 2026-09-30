<#
.SYNOPSIS
  Builds the release exe and packs a Velopack release (Setup.exe + update packages).

.DESCRIPTION
  Output goes to app\Releases. Upload everything in that folder to the update source
  (for GitHub: `gh release create v<version> app\Releases\*`). Versions must increase
  for installed copies to see an update. Requires the `vpk` tool:
    dotnet tool install -g vpk

.EXAMPLE
  .\scripts\release.ps1
  .\scripts\release.ps1 -Version 0.2.0
#>
param(
    [string]$Version,
    [string]$Channel = 'win'
)
$ErrorActionPreference = 'Stop'
$app = Split-Path -Parent $PSScriptRoot
Push-Location $app
try {
    if (-not $Version) {
        $Version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
    }
    Write-Host "Packing Network Monitor $Version"
    if (-not (Get-Command vpk -ErrorAction SilentlyContinue)) { throw 'vpk not found. Run: dotnet tool install -g vpk' }

    cargo test --release
    if ($LASTEXITCODE) { throw 'tests failed' }
    cargo build --release
    if ($LASTEXITCODE) { throw 'build failed' }

    $stage = Join-Path $app 'target\publish'
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
    New-Item -ItemType Directory $stage | Out-Null
    Copy-Item target\release\network-monitor.exe $stage

    vpk pack `
        --packId NetworkMonitor `
        --packVersion $Version `
        --packDir $stage `
        --mainExe network-monitor.exe `
        --packTitle 'Network Monitor' `
        --packAuthors 'fredle' `
        --icon assets\app.ico `
        --channel $Channel `
        --outputDir Releases
    if ($LASTEXITCODE) { throw 'vpk pack failed' }
    Get-ChildItem Releases | Select-Object Name, @{n='MB';e={[math]::Round($_.Length/1MB,2)}}
}
finally { Pop-Location }
