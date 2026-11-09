#!/usr/bin/env bash
# Builds rmx.app for both Apple architectures and packs it into a .dmg in target/dist. Runs on
# macOS; the release workflow is what starts it. The app is signed ad hoc, not notarized.

set -euo pipefail

version=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
targets=(aarch64-apple-darwin x86_64-apple-darwin)
# The oldest macOS the binary runs on, and what Info.plist says.
export MACOSX_DEPLOYMENT_TARGET=11.0

rustup target add "${targets[@]}"
for target in "${targets[@]}"; do
    cargo build --release --locked -p rmx-app --target "$target"
done

dist=target/dist
image=$dist/dmg
app=$image/rmx.app
rm -rf "$image"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

binaries=()
for target in "${targets[@]}"; do
    binaries+=("target/$target/release/rmx")
done
lipo -create -output "$app/Contents/MacOS/rmx" "${binaries[@]}"
sed "s/@VERSION@/$version/" packaging/macos/Info.plist > "$app/Contents/Info.plist"
cp packaging/macos/rmx.icns LICENSE-APACHE NOTICE crates/rmx-ui/assets/LICENSE-*.txt "$app/Contents/Resources/"
# Without any signature an Apple silicon Mac refuses to start it at all.
codesign --force --sign - "$app"

ln -s /Applications "$image/Applications"
hdiutil create -volname rmx -srcfolder "$image" -ov -format UDZO "$dist/rmx-$version-universal.dmg"
rm -rf "$image"
ls -l "$dist"
