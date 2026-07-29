#!/bin/bash
# Send tray test commands to a macOS debug TiyGate desktop process.
#
# After the app has fully started with `make dev-desktop`, run:
#   ./scripts/inject-desktop-tray-loss.sh verify
#
# `verify` performs a complete loss-and-recovery check. `simulate-loss` hides
# the icon only, and `status` compares the tray rectangle returned by macOS
# with the usable menu-bar area (including the notch safe area). The test
# socket is only present in debug builds.
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "This tray-loss injector only supports macOS." >&2
    exit 1
fi

if [[ $# -gt 1 ]]; then
    echo "Usage: $0 [verify|simulate-loss|status]" >&2
    exit 1
fi

command_name="${1:-verify}"
case "$command_name" in
    verify|simulate-loss|status) ;;
    *)
        echo "Unsupported command '$command_name'; use verify, simulate-loss, or status." >&2
        exit 1
        ;;
esac

socket_path="${TIYGATE_DESKTOP_TRAY_TEST_SOCKET:-$HOME/Library/Application Support/ai.tiy.tiygate/tiygate-tray-test.sock}"
if [[ ! -S "$socket_path" ]]; then
    echo "Debug tray test socket is unavailable: $socket_path" >&2
    echo "Start the desktop client with 'make dev-desktop' first." >&2
    exit 1
fi

if ! command -v nc >/dev/null 2>&1; then
    echo "The macOS 'nc' command is required to contact the debug tray test socket." >&2
    exit 1
fi

send_command() {
    local command="$1"
    if ! printf '%s\n' "$command" | nc -U "$socket_path"; then
        echo "Tray test command failed; restart the debug desktop client and try again." >&2
        exit 1
    fi
}

menu_bar_usable_range() {
    # NSStatusItem.visible and a non-empty window rect do not mean that a menu
    # bar item is actually unobscured. On a Mac with a camera housing, AppKit
    # can place an overflowed item underneath the notch. Return physical-pixel
    # bounds for the unobscured right-hand menu-bar segment so the tray rect
    # can be checked in the same coordinate space.
    osascript -l JavaScript -e '
        ObjC.import("Cocoa");
        const screen = $.NSScreen.mainScreen;
        const frame = screen.frame;
        const area = screen.auxiliaryTopRightArea;
        const scale = screen.backingScaleFactor;
        const hasNotch = screen.safeAreaInsets.top > 0 && area.size.width > 0;
        const left = (hasNotch ? area.origin.x : frame.origin.x) * scale;
        const right = (hasNotch
            ? area.origin.x + area.size.width
            : frame.origin.x + frame.size.width) * scale;
        [
            Math.round(left),
            Math.round(right),
            Math.round(scale),
            hasNotch ? 1 : 0
        ].join(" ");
    ' 2>/dev/null
}

diagnose_status() {
    local status="$1"

    if [[ ! "$status" =~ x:\ ([0-9]+),\ y:\ ([0-9]+).*width:\ ([0-9]+),\ height:\ ([0-9]+) ]]; then
        echo "visibility: unknown; could not parse the tray rectangle" >&2
        return 2
    fi

    local tray_x="${BASH_REMATCH[1]}"
    local tray_width="${BASH_REMATCH[3]}"
    local tray_right=$((tray_x + tray_width))
    local range

    if ! range="$(menu_bar_usable_range)"; then
        echo "visibility: unknown; could not read the macOS menu-bar safe area" >&2
        return 2
    fi

    if [[ ! "$range" =~ ^([0-9]+)\ ([0-9]+)\ ([0-9]+)\ ([01])$ ]]; then
        echo "visibility: unknown; macOS returned an unexpected menu-bar safe area" >&2
        return 2
    fi

    local usable_left="${BASH_REMATCH[1]}"
    local usable_right="${BASH_REMATCH[2]}"
    local scale="${BASH_REMATCH[3]}"
    local has_notch="${BASH_REMATCH[4]}"

    if ((tray_x < usable_left || tray_right > usable_right)); then
        local shift_needed=0
        if ((tray_x < usable_left)); then
            shift_needed=$((usable_left - tray_x))
        else
            shift_needed=$((tray_right - usable_right))
        fi
        local points_needed=$(((shift_needed + scale - 1) / scale))

        echo "visibility: hidden by menu-bar overflow; tray=${tray_x}..${tray_right}px, usable-right=${usable_left}..${usable_right}px, notch=${has_notch}"
        echo "action: free at least ${shift_needed}px (${points_needed}pt) of menu-bar space, then restart TiyGate; do not run installed and debug clients together" >&2
        return 1
    fi

    echo "visibility: tray rect is inside the usable right-side menu-bar area; tray=${tray_x}..${tray_right}px, usable-right=${usable_left}..${usable_right}px, notch=${has_notch}"
}

if [[ "$command_name" == "simulate-loss" ]]; then
    send_command "$command_name"
    exit 0
fi

if [[ "$command_name" == "status" ]]; then
    status="$(send_command status)"
    echo "$status"
    diagnosis_result=0
    diagnose_status "$status" || diagnosis_result=$?
    exit "$diagnosis_result"
fi

send_command simulate-loss
echo "waiting 35 seconds for the tray watchdog..."
sleep 35
status="$(send_command status)"
echo "$status"

if [[ "$status" != status:\ tray\ registered\;\ rect=Rect* ]]; then
    echo "Tray recovery did not produce a registered status item with a non-empty rectangle." >&2
    exit 1
fi

diagnosis_result=0
diagnose_status "$status" || diagnosis_result=$?
case "$diagnosis_result" in
    0) ;;
    1)
        echo "Tray recovery recreated the NSStatusItem, but macOS placed it outside the usable menu-bar area." >&2
        exit 1
        ;;
    *)
        echo "Tray recovery recreated the NSStatusItem, but its real menu-bar visibility could not be verified." >&2
        exit 1
        ;;
esac

echo "ok: tray recovery and usable-area geometry verified"
