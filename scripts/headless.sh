#!/usr/bin/env bash
# Run a command in a private, invisible KWin session, so testing rmx never opens a window on the
# desktop. Every XDG directory is a throwaway, which keeps the session away from the real
# compositor and stops KWin from saving its virtual output over the user's display settings.
#
#   scripts/headless.sh target/release/rmx -e sh -c 'ls'
#
# HEADLESS_WIDTH and HEADLESS_HEIGHT set the virtual output's size.

set -euo pipefail

# Under the runtime directory: short enough for a socket path, private, and in memory.
root=$(mktemp -d -p "${XDG_RUNTIME_DIR:-/tmp}" rmx-h.XXXXXX)
trap 'rm -rf "$root"' EXIT
mkdir -m 700 -p "$root"/{run,config,data,cache,state}

# KWin starts its session program from one string, so the arguments travel in a script.
{
    echo '#!/usr/bin/env bash'
    printf 'cd %q\n' "$PWD"
    printf 'exec'
    printf ' %q' "$@"
    echo
} > "$root/inner.sh"
chmod +x "$root/inner.sh"

env -u DISPLAY -u WAYLAND_DISPLAY -u XAUTHORITY -u DBUS_SESSION_BUS_ADDRESS \
    -u SESSION_MANAGER -u XDG_SESSION_TYPE -u XDG_CURRENT_DESKTOP \
    XDG_RUNTIME_DIR="$root/run" \
    XDG_CONFIG_HOME="$root/config" \
    XDG_DATA_HOME="$root/data" \
    XDG_CACHE_HOME="$root/cache" \
    XDG_STATE_HOME="$root/state" \
    dbus-run-session -- kwin_wayland --virtual --no-lockscreen --no-global-shortcuts \
    --no-kactivities --socket rmx-headless \
    --width "${HEADLESS_WIDTH:-2560}" --height "${HEADLESS_HEIGHT:-1600}" \
    --exit-with-session "$root/inner.sh"
