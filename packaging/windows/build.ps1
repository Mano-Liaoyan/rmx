# Builds rmx.exe and packs it into an .msi in target\dist. Runs on Windows; the release workflow
# is what starts it. The installer is unsigned.

$ErrorActionPreference = 'Stop'

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches.Groups[1].Value
# An .msi version is numbers only.
$msiVersion = $version -replace '-.*$', ''

# The C runtime goes into the binary, so that it needs no redistributable installed.
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --locked -p rmx-app --features vendored-openssl
if ($LASTEXITCODE) { exit $LASTEXITCODE }

dotnet tool install --global wix --version 5.0.2
if ($LASTEXITCODE) { exit $LASTEXITCODE }

New-Item -ItemType Directory -Force target\dist | Out-Null
& "$env:USERPROFILE\.dotnet\tools\wix.exe" build packaging\windows\rmx.wxs -arch x64 -b . `
    -d "Version=$msiVersion" -o "target\dist\rmx-$version-x64.msi"
if ($LASTEXITCODE) { exit $LASTEXITCODE }
Get-ChildItem target\dist
