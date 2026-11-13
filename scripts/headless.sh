#!/usr/bin/env bash
# Run a command in a private, invisible compositor, so testing rmx never opens a window on the
# desktop. Every XDG directory is a throwaway, which keeps the session away from the real
# compositor and stops it from saving its virtual output over the user's display settings.
#
#   scripts/headless.sh target/release/rmx -e sh -c 'ls'
#
# HEADLESS_WIDTH and HEADLESS_HEIGHT set the virtual output's size.
#
# HEADLESS_COMPOSITOR picks the compositor:
#   kwin    `kwin_wayland --virtual`, which needs a GPU render node (/dev/dri/renderD*).
#   weston  `weston --backend=headless --renderer=pixman`, which draws on the CPU and so runs
#           anywhere: WSL, containers, CI runners without a GPU.
#   auto    (the default) KWin when there is a render node to use and KWin is installed,
#           otherwise weston.
# Either way the socket is called rmx-headless, and the command runs in a session bus of its own.

set -euo pipefail

width=${HEADLESS_WIDTH:-2560}
height=${HEADLESS_HEIGHT:-1600}

# A render node this user can open, which is what KWin draws with.
render_node() {
    local node
    for node in /dev/dri/renderD*; do
        [[ -r $node && -w $node ]] && return 0
    done
    return 1
}

compositor=${HEADLESS_COMPOSITOR:-auto}
if [[ $compositor == auto ]]; then
    if command -v kwin_wayland >/dev/null && render_node; then
        compositor=kwin
    elif command -v weston >/dev/null; then
        compositor=weston
    elif command -v kwin_wayland >/dev/null; then
        echo "headless.sh: no render node for KWin and no weston to fall back on" >&2
        compositor=kwin
    else
        echo "headless.sh: neither kwin_wayland nor weston is installed" >&2
        exit 1
    fi
fi
case $compositor in
    kwin | weston) ;;
    *)
        echo "headless.sh: HEADLESS_COMPOSITOR is kwin, weston or auto, not '$compositor'" >&2
        exit 1
        ;;
esac

# Under the runtime directory: short enough for a socket path, private, and in memory.
root=$(mktemp -d -p "${XDG_RUNTIME_DIR:-/tmp}" rmx-h.XXXXXX)
mkdir -m 700 -p "$root"/{run,config,data,cache,state}

private=(
    env -u DISPLAY -u WAYLAND_DISPLAY -u XAUTHORITY -u DBUS_SESSION_BUS_ADDRESS
    -u SESSION_MANAGER -u XDG_SESSION_TYPE -u XDG_CURRENT_DESKTOP
    XDG_RUNTIME_DIR="$root/run"
    XDG_CONFIG_HOME="$root/config"
    XDG_DATA_HOME="$root/data"
    XDG_CACHE_HOME="$root/cache"
    XDG_STATE_HOME="$root/state"
)

if [[ $compositor == kwin ]]; then
    trap 'rm -rf "$root"' EXIT
    # KWin starts its session program from one string, so the arguments travel in a script.
    {
        echo '#!/usr/bin/env bash'
        printf 'cd %q\n' "$PWD"
        printf 'exec'
        printf ' %q' "$@"
        echo
    } > "$root/inner.sh"
    chmod +x "$root/inner.sh"

    "${private[@]}" dbus-run-session -- kwin_wayland --virtual --no-lockscreen \
        --no-global-shortcuts --no-kactivities --socket rmx-headless \
        --width "$width" --height "$height" --exit-with-session "$root/inner.sh"
    exit
fi

# Weston has no session program of its own: start it, wait for its socket, run the command
# beside it, and stop it once the command is done.
"${private[@]}" weston --backend=headless --renderer=pixman --width="$width" --height="$height" \
    --socket=rmx-headless --idle-time=0 > "$root/weston.log" 2>&1 &
weston=$!
stop() {
    kill "$weston" 2>/dev/null || true
    wait "$weston" 2>/dev/null || true
    rm -rf "$root"
}
trap stop EXIT

for _ in $(seq 200); do
    [[ -S $root/run/rmx-headless ]] && break
    if ! kill -0 "$weston" 2>/dev/null; then
        echo "headless.sh: weston quit before it was ready:" >&2
        cat "$root/weston.log" >&2
        exit 1
    fi
    sleep 0.05
done
if [[ ! -S $root/run/rmx-headless ]]; then
    echo "headless.sh: weston made no socket in 10 s:" >&2
    cat "$root/weston.log" >&2
    exit 1
fi

status=0
"${private[@]}" WAYLAND_DISPLAY=rmx-headless dbus-run-session -- "$@" || status=$?
exit "$status"
