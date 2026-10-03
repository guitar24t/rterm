# Build the Windows MSI and zip for one architecture.
# Usage: packaging/windows/build-msi.ps1 -Binary <rterm.exe> -Arch x64|arm64 -Version 0.1.5 -Out dist
# Needs the WiX Toolset v5 CLI: dotnet tool install --global wix --version 5.0.2
param(
    [Parameter(Mandatory)] [string] $Binary,
    [Parameter(Mandatory)] [ValidateSet('x64', 'arm64')] [string] $Arch,
    [Parameter(Mandatory)] [string] $Version,
    [Parameter(Mandatory)] [string] $Out
)
$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$label = @{ x64 = 'x86_64'; arm64 = 'aarch64' }[$Arch]

$stage = Join-Path ([IO.Path]::GetTempPath()) "rterm-msi-$PID-$Arch"
Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item $Binary (Join-Path $stage 'rterm.exe')
# One binary, two names: invoked as rterm-connect it runs the session picker.
Copy-Item $Binary (Join-Path $stage 'rterm-connect.exe')
Copy-Item (Join-Path $root 'README.md'), (Join-Path $root 'LICENSE') $stage

New-Item -ItemType Directory -Force $Out | Out-Null
$msi = Join-Path $Out "rterm-$Version-windows-$label.msi"
wix build -arch $Arch -d "Version=$Version" -d "BinDir=$stage" -o $msi (Join-Path $PSScriptRoot 'rterm.wxs')
if ($LASTEXITCODE -ne 0) { throw "wix build failed ($LASTEXITCODE)" }
# WiX also writes debug symbols next to the MSI; they aren't for users.
Remove-Item -Force ([IO.Path]::ChangeExtension($msi, '.wixpdb')) -ErrorAction SilentlyContinue

$zip = Join-Path $Out "rterm-$Version-windows-$label.zip"
Remove-Item -Force $zip -ErrorAction SilentlyContinue
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip
Remove-Item -Recurse -Force $stage
Get-ChildItem $Out | Format-Table Name, Length
