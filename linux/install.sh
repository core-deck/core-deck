#!/usr/bin/env bash
# Install Core Deck binaries to ~/.local/bin, drop the udev rules into
# /etc/udev/rules.d/ (asks for sudo), and run `coredeck setup` to
# register hooks + the systemd user unit.
#
# Shipped at the root of each linux release tarball. Kept here in the
# repo (rather than inlined in release.yml) so it's reviewable and
# covered by the CI shellcheck job.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$HOME/.local/bin"
install -m 0755 "$here/bin/coredeck" "$HOME/.local/bin/coredeck"
install -m 0755 "$here/bin/coredeck-claude" "$HOME/.local/bin/coredeck-claude"
if [ ! -f /etc/udev/rules.d/99-coredeck.rules ]; then
    echo "Installing udev rules (sudo)…"
    sudo install -m 0644 "$here/udev/99-coredeck.rules" /etc/udev/rules.d/99-coredeck.rules
    sudo udevadm control --reload-rules
    sudo udevadm trigger
fi
case ":$PATH:" in
    *":$HOME/.local/bin:"*) ;;
    *) echo "warning: ~/.local/bin is not on \$PATH; add it to your shell rc." ;;
esac
"$HOME/.local/bin/coredeck" setup
echo
echo "Done. Add this to your shell rc to wrap claude:"
echo "    alias claude=\"coredeck-claude\""
