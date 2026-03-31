#!/bin/sh
# Single-app X session: start annotator fullscreen, exit X when it exits.
# startx sets DISPLAY=:0 before invoking this script, but be explicit.
export DISPLAY="${DISPLAY:-:0}"
# Pin SDL2 to the X11 backend so it does not fall back to KMS/Wayland.
export SDL_VIDEODRIVER=x11
export SDL_AUDIODRIVER=dummy
exec /usr/bin/annotator
