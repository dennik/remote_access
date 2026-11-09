#!/bin/sh
# Builds "Remote Access.app" for both Apple Silicon and Intel Macs, and zips it
# for a GitHub release. Run from anywhere: macos/build-app.sh
set -eu
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
out=target/macos
app="$out/Remote Access.app"

for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release -p rd-viewer --target "$target"
done

rm -rf "$out"
mkdir -p "$app/Contents/MacOS"
lipo -create -output "$app/Contents/MacOS/rdviewer" \
    target/aarch64-apple-darwin/release/rdviewer \
    target/x86_64-apple-darwin/release/rdviewer
sed "s/@VERSION@/$version/g" macos/Info.plist > "$app/Contents/Info.plist"

# Ad-hoc signature: required on Apple Silicon. Not notarized, so the installer
# script is the supported way to install it rather than a browser download.
codesign --force --sign - "$app"

(cd "$out" && ditto -c -k --keepParent "Remote Access.app" RemoteAccess-macos.zip)
echo "Built $out/RemoteAccess-macos.zip (version $version)"
