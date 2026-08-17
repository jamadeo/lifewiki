#!/usr/bin/env bash
#
# Record demo.sh with asciinema, keeping the setup out of the recording.
#
#   ./demo/record.sh                    # writes demo/lifewiki.cast
#   PROVIDER=anthropic ./demo/record.sh # hosted model: faster and steadier
#
# Two things this does that recording demo.sh directly does not:
#
#   * warms the model up first, so the recording doesn't open with a minute of
#     silence while 17 GB loads
#   * caps idle time, so the pauses while the model thinks play back quickly
#     without altering the captured timings

set -euo pipefail

cd "$(dirname "$0")/.."
LW="$(pwd)/target/debug/lw"
CAST="${CAST:-demo/lifewiki.cast}"
PROVIDER="${PROVIDER:-local}"
export LIFEWIKI_HOME="${LIFEWIKI_HOME:-/tmp/lifewiki-demo}"

if ! command -v asciinema >/dev/null; then
    echo "asciinema is not installed: brew install asciinema" >&2
    exit 1
fi
if [[ ! -x "$LW" ]]; then
    echo "no lw binary; run: cargo build --features local" >&2
    exit 1
fi

# --- warm up, off camera -----------------------------------------------------

if [[ "$PROVIDER" == "local" ]]; then
    echo "warming up the local model (this is not recorded)…"
    rm -rf "$LIFEWIKI_HOME"
    "$LW" init >/dev/null
    git -C "$LIFEWIKI_HOME" config user.email demo@lifewiki
    git -C "$LIFEWIKI_HOME" config user.name lifewiki
    sed -i '' 's/provider = "anthropic"/provider = "local"/g; /^name = /d' \
        "$LIFEWIKI_HOME/config.toml"
    "$LW" add "warmup" >/dev/null
    "$LW" build -q >/dev/null 2>&1 || true
    echo "model loaded."
fi

# The demo resets the repo itself; the daemon survives because it exits only
# when its socket vanishes, and demo.sh recreates it immediately.
cleanup() {
    "$LW" daemon stop >/dev/null 2>&1 || true
}
trap cleanup EXIT

# --- record ------------------------------------------------------------------

# 90 columns fits wiki pages and the agent's indented output without wrapping.
# --idle-time-limit caps playback pauses: the model sometimes thinks for 45
# seconds, which is honest but unwatchable. 0.8s keeps the whole thing near a
# minute. The captured timings are unchanged, so IDLE can be retuned afterwards
# with `asciinema convert` rather than re-recording.
SKIP_WARMUP=1 KEEP_DAEMON=1 asciinema rec \
    --headless \
    --overwrite \
    --window-size 90x30 \
    --idle-time-limit "${IDLE:-0.8}" \
    --title "lifewiki — tell it about your life, it writes the wiki" \
    --command ./demo/demo.sh \
    "$CAST"

echo
echo "recorded: $CAST"
python3 - "$CAST" <<'PY'
import json, sys
lines = open(sys.argv[1]).read().splitlines()
events = [json.loads(l) for l in lines[1:] if l.strip()]
raw = sum(e[0] for e in events)
# Playback applies the idle cap, so estimate what a viewer actually sits through.
import os
cap = float(os.environ.get("IDLE", "0.8"))
capped = sum(min(e[0], cap) for e in events)
print(f"  captured {raw:.0f}s, plays back in about {capped:.0f}s")
PY
echo
echo "watch it:   asciinema play $CAST"
echo "share it:   asciinema upload $CAST"
echo "make a gif: brew install agg && agg $CAST demo/lifewiki.gif"
