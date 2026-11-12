#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Build the release archive from an existing `cargo build --release` in src/:
#   packaging/make-release.sh [VERSION]   ->  dist/micdock-VERSION-x86_64-linux.tar.gz (+ .sha256)
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
version=${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/src/Cargo.toml" | head -1)}
arch=$(uname -m)
name="micdock-$version-$arch-linux"
out="$root/dist"
stage="$out/$name"

rm -rf "$stage"
mkdir -p "$stage/bin" "$stage/share/micdock" "$stage/share/applications"
for b in micdock rodego2 ugg2wav micdock-firmware; do
    install -m 755 "$root/src/target/release/$b" "$stage/bin/$b"
done
cp -r "$root/docs" "$stage/share/micdock/docs"
cp "$root/micdock.desktop" "$stage/share/applications/"
cp "$root/70-rode.rules" "$root/README.md" "$root/LICENSE" "$root/PROTOCOL.md" "$stage/"
install -m 755 "$root/packaging/install.sh" "$stage/install.sh"

tar -C "$out" -czf "$out/$name.tar.gz" "$name"
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
rm -rf "$stage"
echo "$out/$name.tar.gz"
