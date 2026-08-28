<#
.SYNOPSIS
    Builds the OTWONO AI Windows installers.

.DESCRIPTION
    Tauri's NSIS and MSI bundlers need Windows tooling, so this script must run
    on Windows. It is the only supported way to produce the .exe and .msi; the
    Linux and macOS bundles are produced by `npm run desktop:build` on those
    platforms.

    The script checks its prerequisites first and says exactly what is missing
    rather than failing part-way through a long build.

.PARAMETER OutputDirectory
    Where to copy the finished installers and their checksums.

.EXAMPLE
    pwsh -File scripts/build-windows.ps1
#>

[CmdletBinding()]
param(
    [string] $OutputDirectory = "releases/windows"
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-Command {
    param([string] $Name, [string] $Hint)
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        throw "$Name was not found. $Hint"
    }
}

Write-Host 'Checking prerequisites…'
Assert-Command 'node'  'Install Node.js 20 or later from https://nodejs.org.'
Assert-Command 'npm'   'Node.js includes npm; reinstall Node.js.'
Assert-Command 'cargo' 'Install Rust from https://rustup.rs.'

$repository = Split-Path -Parent $PSScriptRoot
Push-Location $repository

try {
    Write-Host 'Installing dependencies…'
    npm ci

    Write-Host 'Checking types and running tests…'
    npm run typecheck
    npm run test
    cargo test --workspace

    Write-Host 'Building the desktop application…'
    # Tauri builds the web assets first, through beforeBuildCommand.
    npm run desktop:build

    $bundleRoot = Join-Path $repository 'target/release/bundle'
    if (-not (Test-Path $bundleRoot)) {
        throw "The build produced no bundle directory at $bundleRoot."
    }

    $destination = Join-Path $repository $OutputDirectory
    New-Item -ItemType Directory -Force -Path $destination | Out-Null

    $installers = Get-ChildItem -Path $bundleRoot -Recurse -Include '*.exe', '*.msi'
    if ($installers.Count -eq 0) {
        throw 'The build finished but produced no .exe or .msi. Check the Tauri output above.'
    }

    foreach ($installer in $installers) {
        Copy-Item $installer.FullName -Destination $destination -Force
        $copied = Join-Path $destination $installer.Name
        $hash = (Get-FileHash -Algorithm SHA256 -Path $copied).Hash.ToLower()
        "$hash  $($installer.Name)" | Set-Content -Path "$copied.sha256" -Encoding ascii
        Write-Host "  $($installer.Name)"
        Write-Host "  SHA-256: $hash"
    }

    Write-Host ''
    Write-Host "Installers are in $destination"
    Write-Host 'These builds are unsigned. Windows will warn about an unknown publisher until'
    Write-Host 'they are signed with a code-signing certificate — see docs/RELEASE.md.'
}
finally {
    Pop-Location
}
