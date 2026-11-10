# Points the build at a prebuilt OpenSSL instead of compiling one: SQLCipher links its libcrypto
# statically, so rmx.exe still needs no DLL beside it. Dot-source it before `cargo build`; under
# GitHub Actions it also writes the variables to $GITHUB_ENV for the later steps.
#
# It uses the Win64 OpenSSL from slproweb.com (winget ShiningLight.OpenSSL.Dev, choco openssl),
# installing it with Chocolatey if none is found. Its static library is built against the static
# C runtime, which is why the build sets `+crt-static`.

$ErrorActionPreference = 'Stop'

function Find-OpenSsl {
    $candidates = @($env:OPENSSL_DIR, "$env:ProgramFiles\OpenSSL", "$env:ProgramFiles\OpenSSL-Win64")
    foreach ($dir in $candidates) {
        if ($dir -and (Test-Path "$dir\include\openssl\ssl.h") -and (Test-Path "$dir\lib\VC\x64\MT\libcrypto_static.lib")) {
            return $dir
        }
    }
}

$root = Find-OpenSsl
if (-not $root) {
    choco install openssl --no-progress -y
    if ($LASTEXITCODE) { exit $LASTEXITCODE }
    $root = Find-OpenSsl
    if (-not $root) { throw 'OpenSSL was installed but its static libcrypto is not where expected' }
}

# libsqlite3-sys links `libcrypto`; give it the static library under that name.
$lib = Join-Path (Get-Location) 'target\openssl'
New-Item -ItemType Directory -Force $lib | Out-Null
Copy-Item "$root\lib\VC\x64\MT\libcrypto_static.lib" "$lib\libcrypto.lib" -Force

$vars = @{
    OPENSSL_DIR         = $root
    OPENSSL_INCLUDE_DIR = "$root\include"
    OPENSSL_LIB_DIR     = $lib
    # The static C runtime, and what a static libcrypto needs from Windows.
    RUSTFLAGS           = '-C target-feature=+crt-static -C link-arg=crypt32.lib -C link-arg=ws2_32.lib -C link-arg=advapi32.lib -C link-arg=user32.lib'
}
foreach ($name in $vars.Keys) {
    Set-Item "env:$name" $vars[$name]
    if ($env:GITHUB_ENV) { "$name=$($vars[$name])" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8 }
}
Write-Host "Using OpenSSL from $root"
