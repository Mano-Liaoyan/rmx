#!/bin/sh
# The whole desktop: one terminal, with no window manager so the pointer decides focus.
# Typed text ends up in ~/typed.txt for the test to read through `exec`.
exec xterm -geometry 160x50+0+0 -e sh -c 'stty -echo; exec cat > "$HOME/typed.txt"'
