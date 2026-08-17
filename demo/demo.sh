#!/usr/bin/env bash
#
# A recorded walkthrough: capture a few notes, build the wiki, ask a question.
#
#   ./demo/demo.sh              # run it
#   asciinema rec -c ./demo/demo.sh lifewiki.cast
#
# Runs against a throwaway repo in /tmp, so it never touches ~/.lifewiki.
#
# Set DEMO_SPEED=0 to remove the typing animation, or PROVIDER=anthropic to
# record against a hosted model instead of the local one.

set -euo pipefail

LW="${LW:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/lw}"
export LIFEWIKI_HOME="${LIFEWIKI_HOME:-/tmp/lifewiki-demo}"
PROVIDER="${PROVIDER:-local}"
DEMO_SPEED="${DEMO_SPEED:-0.035}"   # seconds per character
PAUSE="${PAUSE:-1.2}"               # beat between steps

if [[ ! -x "$LW" ]]; then
    echo "no lw binary at $LW" >&2
    echo "build one first: cargo build --features local" >&2
    exit 1
fi

bold=$'\033[1m'; dim=$'\033[2m'; cyan=$'\033[36m'; green=$'\033[32m'; off=$'\033[0m'

# Type a command out, then run it. The animation is what makes a recording feel
# live rather than like a wall of output.
#
# What is displayed is not quite what runs: the binary shows as `lw` rather than
# an absolute path, and arguments with spaces are shown quoted, so a viewer sees
# a line they could actually type.
run() {
    local shown=() part
    for part in "$@"; do
        case "$part" in
            "$LW") shown+=("lw") ;;
            "$LIFEWIKI_HOME"*) shown+=("${part/$LIFEWIKI_HOME/~/.lifewiki}") ;;
            *" "*) shown+=("\"$part\"") ;;
            *) shown+=("$part") ;;
        esac
    done

    printf '%s$ %s' "$green" "$off"
    if [[ "$DEMO_SPEED" == "0" ]]; then
        printf '%s%s%s\n' "$bold" "${shown[*]}" "$off"
    else
        local text="${shown[*]}" i
        for ((i = 0; i < ${#text}; i++)); do
            printf '%s%s%s' "$bold" "${text:i:1}" "$off"
            sleep "$DEMO_SPEED"
        done
        printf '\n'
    fi
    sleep 0.3
    # A failure here should not end the take: the agent is non-deterministic and
    # a missing page is better shown than fatal.
    "$@" || printf '%s(command failed)%s\n' "$dim" "$off"
    printf '\n'
    sleep "$PAUSE"
}

# The agent chooses its own paths, and may write several pages in one build. Find
# the page *about Ada* rather than assuming /people/ada.md or taking whichever
# file grep happens to reach first.
find_ada() {
    grep -rl '^title: Ada' "$LIFEWIKI_HOME/wiki" 2>/dev/null | head -1
}

# A narrator line, so a viewer knows why the next command matters.
say() {
    printf '%s# %s%s\n\n' "$dim" "$*" "$off"
    sleep 1.0
}

# --- setup, before recording starts ------------------------------------------

# Reset the repo's contents while leaving the daemon and its socket alone.
#
# The socket lives inside LIFEWIKI_HOME, so `rm -rf` on the whole directory
# would delete it out from under a running daemon and every later request would
# fail with a decode error. Clear only what the demo writes.
reset_repo() {
    rm -rf "$LIFEWIKI_HOME/log" "$LIFEWIKI_HOME/wiki" "$LIFEWIKI_HOME/state" \
           "$LIFEWIKI_HOME/.git"
    "$LW" init >/dev/null
    git -C "$LIFEWIKI_HOME" config user.email demo@lifewiki
    git -C "$LIFEWIKI_HOME" config user.name lifewiki
}

rm -rf "$LIFEWIKI_HOME"
"$LW" init >/dev/null
git -C "$LIFEWIKI_HOME" config user.email demo@lifewiki
git -C "$LIFEWIKI_HOME" config user.name lifewiki
if [[ "$PROVIDER" == "local" ]]; then
    sed -i '' 's/provider = "anthropic"/provider = "local"/g; /^name = /d' \
        "$LIFEWIKI_HOME/config.toml"

    # Load the model before the demo proper, or the first command sits for a
    # minute loading 17 GB. This has to happen in the demo's own home, since the
    # daemon is per-repo.
    #
    # SKIP_WARMUP=1 means someone already did this (see demo/record.sh), which
    # keeps a minute of nothing out of the recording.
    if [[ "${SKIP_WARMUP:-0}" != "1" ]]; then
        printf '%swarming up the local model…%s\n' "$dim" "$off"
        "$LW" add "warmup" >/dev/null
        "$LW" build -q >/dev/null 2>&1 || true
        reset_repo
    fi
fi

# Leave no daemon behind when the demo ends: it would hold ~17 GB. When a
# recording wrapper owns the daemon it stops it itself, so skip that here.
if [[ "${KEEP_DAEMON:-0}" != "1" ]]; then
    trap '"$LW" daemon stop >/dev/null 2>&1 || true' EXIT
fi

clear

# --- the demo ---------------------------------------------------------------

printf '%slifewiki%s — tell it things about your life; it writes the wiki.\n\n' \
    "$bold" "$off"
sleep 1.5

say "Jot things down as they happen. No tags, no categories — just type."

run "$LW" add "Ada is my sister. She lives in Lisbon and works as an architect."
run "$LW" add "Had lunch with Ada on Sunday. She's thinking about moving."

say "Each entry is committed to git the moment it exists."

run "$LW" log

say "Now fold the log into a wiki."

run "$LW" build

say "It made a page for Ada. Note where it says she lives."

# Show the claims before the contradiction arrives, so the rewrite is visible.
# Only the prose: the frontmatter is shown in full after the second build.
ada_page="$(find_ada)"
if [[ -n "$ada_page" ]]; then
    run sed -n '/^# /,$p' "$ada_page"
fi

say "Facts change. You don't edit anything — you just say what's true now."

run "$LW" add "Ada moved to Porto last month."

say "Build again. It has to notice this contradicts the page it just wrote."

run "$LW" build

say "It rewrote the page rather than adding a second, conflicting one."

ada_page="$(find_ada)"
if [[ -n "$ada_page" ]]; then
    run cat "$ada_page"
else
    printf '%s(no page for Ada — the build did not go as expected)%s\n\n' "$dim" "$off"
fi

say "Now ask it something. It searches the wiki and cites what it used."

run "$LW" ask "where does Ada live, and what does she do?"

say "And when the wiki doesn't know, it says so instead of guessing."

run "$LW" ask -q "what is Ada's phone number?"

say "The log is the source of truth. The wiki is derived, and in git."

cd "$LIFEWIKI_HOME"
run git log --oneline

printf '%sgithub.com/jackamadeo/lifewiki%s\n' "$bold" "$off"
sleep 2
