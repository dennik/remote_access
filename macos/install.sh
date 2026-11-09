#!/bin/sh
# Installs or updates the Remote Access viewer on a Mac:
#   curl -fsSL <url of this file> | sh
# Puts "Remote Access.app" in ~/Applications and an `rdviewer` command in ~/.local/bin.
# Needs no admin password.
set -eu

: "${REMOTE_ACCESS_ZIP_URL:=https://github.com/dennik/remote_access/releases/latest/download/RemoteAccess-macos.zip}"
apps="$HOME/Applications"
bin="$HOME/.local/bin"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading Remote Access…"
curl -fsSL "$REMOTE_ACCESS_ZIP_URL" -o "$tmp/app.zip"
ditto -x -k "$tmp/app.zip" "$tmp"

mkdir -p "$apps" "$bin"
rm -rf "$apps/Remote Access.app"
mv "$tmp/Remote Access.app" "$apps/"
# curl downloads aren't quarantined, but clear it in case the zip came another way.
xattr -dr com.apple.quarantine "$apps/Remote Access.app" 2>/dev/null || true
ln -sf "$apps/Remote Access.app/Contents/MacOS/rdviewer" "$bin/rdviewer"

echo "Installed Remote Access in $apps."
case ":$PATH:" in
    *":$bin:"*) ;;
    *)
        echo "export PATH=\"\$HOME/.local/bin:\$PATH\"" >> "$HOME/.zprofile"
        echo "Added $bin to your PATH. Open a new terminal window to use the rdviewer command."
        ;;
esac
echo
echo "Open it from Launchpad or Spotlight (\"Remote Access\"), or run: rdviewer"
echo "Your admin key, for 'rdagent.exe install --admin-key' on each Windows PC:"
"$bin/rdviewer" key
