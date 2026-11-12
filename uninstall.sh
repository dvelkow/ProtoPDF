#!/usr/bin/env bash
# Removes what install.sh added. PDF files go back to the system's default app.
set -uo pipefail
bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
exe="$bin/protopdf"
if [ -x "$exe" ]; then
    "$exe" --unregister
    rm -f "$exe"
fi
echo "ProtoPDF removed."
