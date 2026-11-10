# Builds rmx.exe and packs it into an .msi in target\dist. Runs on Windows; the release workflow
# is what starts it. The installer is unsigned.

$ErrorActionPreference = 'Stop'

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches.Groups[1].Value
# An .msi version is numbers only.
$msiVersion = $version -replace '-.*$', ''

# A prebuilt OpenSSL, linked statically along with the C runtime, so that the binary needs no
# DLL or redistributable installed.
. "$PSScriptRoot\openssl.ps1"
cargo build --release --locked -p rmx-app
if ($LASTEXITCODE) { exit $LASTEXITCODE }

dotnet tool install --global wix --version 5.0.2
if ($LASTEXITCODE) { exit $LASTEXITCODE }

New-Item -ItemType Directory -Force target\dist | Out-Null
& "$env:USERPROFILE\.dotnet\tools\wix.exe" build packaging\windows\rmx.wxs -arch x64 -b . `
    -d "Version=$msiVersion" -o "target\dist\rmx-$version-x64.msi"
if ($LASTEXITCODE) { exit $LASTEXITCODE }
Get-ChildItem target\dist
