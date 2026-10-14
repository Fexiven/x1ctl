#!/usr/bin/env bash
# Hardware test for x1ctl on a real Sound Blaster X1.
# Writes everything (including every USB frame) to ./x1-hwtest.log — attach that file.
#
#   ./x1-hwtest.sh            read-only checks only
#   ./x1-hwtest.sh --writes   also toggle Super X-Fi / EQ / mute and restore them
#
set -u
cd "$(dirname "$0")"
LOG=$PWD/x1-hwtest.log
X=$PWD/x1ctl
WRITES=0; [ "${1:-}" = "--writes" ] && WRITES=1
: > "$LOG"

say() { echo -e "\n### $*" | tee -a "$LOG"; }
run() { echo "\$ x1ctl $*" | tee -a "$LOG"; RUST_LOG=x1=trace "$X" --direct --port "$PORT" "$@" 2>>"$LOG" | tee -a "$LOG"; return "${PIPESTATUS[0]}"; }

say "environment"
{ uname -a; [ -f /etc/os-release ] && . /etc/os-release && echo "$PRETTY_NAME"; } | tee -a "$LOG"
lsusb 2>/dev/null | grep -i 041e | tee -a "$LOG"

PORT="${X1_PORT:-}"
[ -z "$PORT" ] && for t in /sys/class/tty/ttyACM*; do
  [ -e "$t" ] || continue
  d=$(readlink -f "$t/device")
  for _ in 1 2 3 4; do
    if [ -f "$d/idVendor" ]; then
      [ "$(cat "$d/idVendor"):$(cat "$d/idProduct")" = "041e:3286" ] && PORT=/dev/$(basename "$t")
      break
    fi
    d=$(dirname "$d")
  done
done
if [ -z "$PORT" ]; then
  say "no ttyACM for 041e:3286 found — is the X1 plugged in and is the cdc_acm module loaded?"
  ls -l /dev/ttyACM* 2>&1 | tee -a "$LOG"; exit 1
fi
ls -l "$PORT" | tee -a "$LOG"
id | tee -a "$LOG"
if [ ! -r "$PORT" ] || [ ! -w "$PORT" ]; then
  say "no permission on $PORT. Quick fix for this test:  sudo chmod a+rw $PORT   (or run this script with sudo)"
  exit 1
fi
if systemctl is-active --quiet ModemManager 2>/dev/null; then
  say "WARNING: ModemManager is running and may probe $PORT with AT commands; if the test fails, run: sudo systemctl stop ModemManager"
fi
if command -v busctl >/dev/null && busctl --user status io.github.x1ctl >/dev/null 2>&1; then
  say "x1d is running; stop it for this test (systemctl --user stop x1d)"; exit 1
fi

say "1. status (handshake, auth, all read commands)"
run status || { say "status FAILED — please attach $LOG"; exit 1; }
say "2. status again (device should now already be in binary mode, no auth)"
run status --json
say "3. super x-fi info"
run sxfi-info
say "4. listen for 15 s: press the SXFI button twice and turn the volume knob a bit"
( RUST_LOG=x1=trace timeout 15 "$X" --direct --port "$PORT" monitor 2>>"$LOG" | tee -a "$LOG" ) || true

if [ $WRITES = 1 ]; then
  say "5. reversible writes"
  S0=$("$X" --direct --port "$PORT" sxfi); E0=$("$X" --direct --port "$PORT" status --json)
  run sxfi toggle; sleep 1
  run sxfi "$S0"
  BAND=$(echo "$E0" | python3 -c 'import json,sys; print(json.load(sys.stdin)["bands"][5])' 2>/dev/null || echo 0)
  run eq set 1k 3; run eq set 1k "$BAND"
  M0=$("$X" --direct --port "$PORT" mute)
  run mute toggle; run mute "$M0"
  V0=$("$X" --direct --port "$PORT" volume | sed 's/ dB//')
  run volume -- "$(python3 -c "print($V0-1)" 2>/dev/null || echo -20)"
  run volume -- "$V0"
  say "final state"
  run status
fi
say "done — please attach $LOG"
