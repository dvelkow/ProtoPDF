#!/usr/bin/env bash
# Installs the built ProtoPDF for the current Linux user (no root needed):
# copies the program to ~/.local/bin, adds it to the application menu and
# makes it the default PDF app. Run again after every rebuild:
#     ./install.sh               install and make it the default for PDF files
#     ./install.sh --no-default  install, but leave the default PDF app alone
set -euo pipefail
cd "$(dirname "$0")"

src=target/release/ProtoPDF
[ -x "$src" ] || { echo "Build first: cargo build --release" >&2; exit 1; }

bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
exe="$bin/protopdf"
mkdir -p "$bin"
# Copy next to the target and rename it into place: windows that are open keep
# running the old file and the next start uses the new one.
tmp="$bin/.protopdf.new.$$"
cp "$src" "$tmp"
chmod 755 "$tmp"
mv -f "$tmp" "$exe"

# Writes the desktop entry (application menu, "Open With") and the icon.
# Without --quiet it also makes ProtoPDF the program that opens PDF files.
if [ "${1:-}" = "--no-default" ]; then
    "$exe" --register --quiet
else
    "$exe" --register
fi

echo "Installed: $exe"
echo "Menu entry: ${XDG_DATA_HOME:-$HOME/.local/share}/applications/protopdf.desktop"
