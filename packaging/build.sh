#!/usr/bin/env bash
# Builds the binary once and packs it into a .deb and an .rpm in /out. Runs inside the image from
# packaging/Containerfile, in a private copy of the source tree; `cargo run -p xtask -- package`
# is what starts it.

set -euo pipefail
umask 022

version=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
# A tilde is how both formats sort a pre-release before its release.
package_version=${version//-/\~}
icon_sizes=(16 22 24 32 48 64 128 256 512)
binary=target/release/rmx
licences=(LICENSE-APACHE NOTICE crates/rmx-ui/assets/LICENSE-Geist-OFL.txt
    crates/rmx-ui/assets/LICENSE-Lucide-ISC.txt)

mkdir -p /out
desktop-file-validate packaging/rmx.desktop

nice -n 19 cargo build --release --locked -p rmx-app --features vendored-openssl

# The dependency lists in deb-control and rmx.spec are written by hand from this set.
expected="ld-linux-x86-64.so.2 libc.so.6 libfontconfig.so.1 libfreetype.so.6 libgcc_s.so.1 libm.so.6 libz.so.1"
needed=$(readelf -d "$binary" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' | sort | xargs)
if [ "$needed" != "$expected" ]; then
    echo "the binary links [$needed], the package metadata was written for [$expected]" >&2
    exit 1
fi

max_glibc=$(objdump -T "$binary" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -n 1)
max_glibc=${max_glibc#GLIBC_}

# Writes the files both packages share under $1, with the licences in $2.
stage() {
    local root=$1 licence_dir=$2 size icons=$1/usr/share/icons/hicolor
    install -Dm755 "$binary" "$root/usr/bin/rmx"
    install -Dm644 packaging/rmx.desktop "$root/usr/share/applications/rmx.desktop"
    install -Dm644 packaging/rmx.svg "$icons/scalable/apps/rmx.svg"
    for size in "${icon_sizes[@]}"; do
        install -d "$icons/${size}x$size/apps"
        rsvg-convert -w "$size" -h "$size" packaging/rmx.svg -o "$icons/${size}x$size/apps/rmx.png"
    done
    install -Dm644 -t "$root/$licence_dir" "${licences[@]}"
}

deb=/work/deb
stage "$deb" usr/share/doc/rmx
install -Dm644 packaging/deb-copyright "$deb/usr/share/doc/rmx/copyright"
installed_size=$(du -sk "$deb" | cut -f1)
mkdir "$deb/DEBIAN"
sed -e "s/@VERSION@/$package_version/" -e "s/@GLIBC@/$max_glibc/" \
    -e "s/@INSTALLED_SIZE@/$installed_size/" packaging/deb-control > "$deb/DEBIAN/control"
# xz, which every dpkg can read; older ones cannot read zstd.
dpkg-deb --root-owner-group -Zxz --build "$deb" "/out/rmx_${package_version}-1_amd64.deb"

rpm_payload=/work/rpm-payload
stage "$rpm_payload" usr/share/licenses/rmx
rpmbuild -bb --target x86_64 \
    --define "_topdir /work/rpm" \
    --define "_rpmdir /out" \
    --define "_rpmfilename %%{NAME}-%%{VERSION}-%%{RELEASE}.%%{ARCH}.rpm" \
    --define "rmx_version $package_version" \
    --define "rmx_payload $rpm_payload" \
    packaging/rmx.spec

echo "rmx $version: max GLIBC symbol version $max_glibc" >&2
ls -l /out >&2
