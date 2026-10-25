#!/usr/bin/env bash
# Phase 1 step 9: what comes out of the sink. Response against the daemon's curve, the null test,
# a glitch run (band changes 20x/s, A/B, preamp), and the CPU budget. Private test sink only
# (omadsp_spike_out), 48 kHz; config and state go to a scratch directory.
#   GLITCH_SECS=60 (default 30)   length of each glitch run
#   ONLY=response|glitch|q256     run one section (q256: just the quantum-256 glitch run)
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=../../spikes/lib/common.sh
source "$ROOT/spikes/lib/common.sh"
OUT="$RUN/dsp"
rm -rf "$OUT"
mkdir -p "$OUT/rt"
cargo build -q --manifest-path "$ROOT/Cargo.toml" -p omadspd -p omadsp
D="$ROOT/target/debug/omadspd"
C="$ROOT/target/debug/omadsp"
export OMADSP_SOCKET="$OUT/rt/omadsp.sock"
GLITCH_SECS="${GLITCH_SECS:-30}"

baseline
SINK="$(sink_start)"
wait_node omadsp_spike_out >/dev/null
PLAY="" DPID=""
cleanup() {
  [[ -n $PLAY ]] && kill "$PLAY" 2>/dev/null
  [[ -n $DPID ]] && kill "$DPID" 2>/dev/null && wait "$DPID" 2>/dev/null
  sink_stop "$SINK"
  sleep 0.3
  check_clean || true
}
trap cleanup EXIT

PASS=0 FAIL=0
ok() { echo "PASS $*"; PASS=$((PASS + 1)); }
bad() { echo "FAIL $*"; FAIL=$((FAIL + 1)); }
jq_() { python3 -I -c "import json,sys; d=json.load(sys.stdin); print($1)"; }

"$D" --name omadsp_test --target omadsp_spike_out --runtime-dir "$OUT/rt" --config-dir "$OUT/config" \
  --state-dir "$OUT/state" -v >"$OUT/daemon.log" 2>&1 &
DPID=$!
for _ in $(seq 1 50); do "$C" status >/dev/null 2>&1 && break; sleep 0.1; done
wait_running() {
  for _ in $(seq 1 60); do "$C" status --json | grep -q '"engine": "running"' && return 0; sleep 0.1; done
  return 1
}

# curve_json <out>: the daemon's left-channel curve as {freq, db}
curve_json() {
  python3 -I - "$OMADSP_SOCKET" "$1" <<'PY'
import json, socket, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); f = s.makefile("rb")
for line in f:
    m = json.loads(line)
    if m.get("event") == "curve.updated":
        p = m["payload"]
        json.dump({"freq": p["freqs"], "db": p["left"]}, open(sys.argv[2], "w"))
        break
PY
}

# record <wav> <secs> <player wav> <play target>: play <wav> while recording the sink
record() {
  local rec
  rec="$(rec_start_raw "$1" omadsp_spike_out)"
  sleep 0.3
  play "$3" "$4" &
  PLAY=$!
  sleep "$2"
  wait "$PLAY" 2>/dev/null
  PLAY=""
  sleep 0.3
  rec_stop "$rec"
}

signals multi "$OUT/multi.wav" --db -30 --secs 8 --lead 0.3
signals pink "$OUT/pink.wav" --db -20 --secs 8 --lead 0.3
signals sine "$OUT/tone.wav" --freq 1000 --db -20 --secs "$((GLITCH_SECS + 8))" --lead 0

if [[ -z ${ONLY:-} || $ONLY == response ]]; then
echo "== 1. response vs the daemon's curve (manual preamp 0 dB; multitone at -30 dBFS)"
"$C" on; wait_running && ok "engine running" || bad "engine never ran"
"$C" preamp 0 --manual
check_response() {
  local label="$1" tol="${2:-0.1}" r
  sleep 0.3
  NRESP=$((${NRESP:-0} + 1))
  curve_json "$OUT/curve$NRESP.json"
  record "$OUT/resp$NRESP.wav" 8.5 "$OUT/multi.wav" omadsp_test
  r="$(measure response "$OUT/multi.wav" "$OUT/resp$NRESP.wav" --expect "$OUT/curve$NRESP.json" --quiet)"
  echo "RESULT $label $r"
  python3 -I -c "import json,sys; sys.exit(0 if json.loads(sys.argv[1])['max_abs_err_db'] <= float(sys.argv[2]) else 1)" "$r" "$tol" \
    && ok "$label within $tol dB" || bad "$label off the curve: $r"
}
"$C" preset Flat
"$C" preamp 0 --manual
for i in 1 2 3 4 5; do "$C" band "$i" "$((i * 2))"; done
for i in 6 7 8 9 10; do "$C" band "$i" "$(((11 - i) * 2))"; done
check_response "graphic V (10 bands)"
"$C" layout --bands 32
for i in $(seq 1 32); do "$C" band "$i" "$(((i * 7) % 9 - 4))"; done
# +-4 dB band to band: the slopes below 100 Hz reach 2 dB per curve point, so the curve's own
# log-frequency interpolation and the 1 Hz measurement bins add up to ~0.16 dB there.
check_response "32 bands" 0.2
"$C" reset
"$C" tone --bass 5 --treble -4
check_response "tone shelves +5/-4"
"$C" tone --bass 0 --treble 0

echo "== 2. null: DSP on, flat, vs DSP off"
"$C" preset Flat
"$C" preamp 0 --manual
"$C" off; sleep 1
record "$OUT/null-off.wav" 8.5 "$OUT/pink.wav" omadsp_spike_out
"$C" on; wait_running; sleep 0.5
record "$OUT/null-on.wav" 8.5 "$OUT/pink.wav" omadsp_test
n="$(measure null "$OUT/null-off.wav" "$OUT/null-on.wav")"
echo "RESULT null $n"
python3 -I -c "import json,sys; sys.exit(0 if json.loads(sys.argv[1])['diff_peak_db'] <= -120 else 1)" "$n" \
  && ok "flat chain nulls to <= -120 dBFS" || bad "flat chain does not null: $n"

fi

echo "== 3. glitch: band $((GLITCH_SECS * 20)) changes at 20/s, A/B toggles, preamp changes, ${GLITCH_SECS} s"
glitch() {
  "$C" preset Flat; "$C" preamp 0 --manual; "$C" on; wait_running; sleep 0.5
  local tag="$1" lat=("${@:2}") start end i=6 k err0 rec
  err0="$(grep -c '^\[E\]' "$OUT/daemon.log")"
  rec="$(rec_start_raw "$OUT/glitch-$tag.wav" omadsp_spike_out)"
  sleep 0.3
  : >"$OUT/marks-$tag.txt"
  mark() { echo "$(python3 -I -c "import time; print(round(time.time() - $t0, 3))") $*" >>"$OUT/marks-$tag.txt"; }
  t0="$(date +%s.%N)"
  play "$OUT/tone.wav" omadsp_test "${lat[@]}" &
  PLAY=$!
  sleep 1
  start=$SECONDS
  # whole triangle cycles, so band 5 ends where it began (0 dB)
  while ((SECONDS - start < GLITCH_SECS || i % 24 != 6)); do
    k=$((i % 24)); "$C" band 5 "$((k < 12 ? k - 6 : 18 - k))"  # a triangle: 1 dB per step, like a drag
    ((i % 100 == 50)) && { mark ab; "$C" ab; }
    ((i % 100 == 99)) && { mark ab-off; "$C" ab off; }
    ((i % 70 == 35)) && { mark preamp; "$C" preamp "$((-(i % 5)))" --manual; }
    i=$((i + 1))
    sleep 0.05
  done
  mark final; "$C" ab off; "$C" preamp 0 --manual
  sleep 1.5
  kill "$PLAY" 2>/dev/null; wait "$PLAY" 2>/dev/null; PLAY=""
  rec_stop "$rec"
  end="$(grep -c '^\[E\]' "$OUT/daemon.log")"
  c="$(measure clicks "$OUT/glitch-$tag.wav" --freq 1000 --top 12)"
  cat "$OUT/marks-$tag.txt" | tr '\n' ';'; echo
  echo "RESULT glitch $tag: $i commands, $((end - err0)) PipeWire [E] lines, clicks $c"
  # the stream's own start (first second, before the first command) is not a command's click
  python3 -I -c "
import json, sys
c = json.loads(sys.argv[1])
bad = [w for w in c.get('top_s_dBc', []) if w[0] >= 1.0]
print('RESULT glitch clicks after the first second:', bad)
sys.exit(1 if bad else 0)" "$c" && ok "glitch $tag: no click over -60 dBc" \
    || { bad "glitch $tag: clicks"; echo "RESULT glitch $tag gaps: $(measure gaps "$OUT/glitch-$tag.wav" --min-ms 1)"; }
}
[[ -z ${ONLY:-} || $ONLY == glitch ]] && glitch default
glitch q256 --latency 256

if [[ -z ${ONLY:-} ]]; then
echo "== 4. budget: CPU of the daemon and the host while a tone plays"
HOST="$(pgrep -P "$DPID" | head -1)"
ticks() { awk '{print $14 + $15}' "/proc/$1/stat"; }
play "$OUT/tone.wav" omadsp_test &
PLAY=$!
sleep 2
t0d="$(ticks "$DPID")" t0h="$(ticks "$HOST")"
sleep 10
t1d="$(ticks "$DPID")" t1h="$(ticks "$HOST")"
kill "$PLAY" 2>/dev/null; wait "$PLAY" 2>/dev/null; PLAY=""
hz="$(getconf CLK_TCK)"
cpu_d="$(python3 -I -c "print(round(($t1d - $t0d) / $hz / 10 * 100, 2))")"
cpu_h="$(python3 -I -c "print(round(($t1h - $t0h) / $hz / 10 * 100, 2))")"
echo "RESULT budget daemon ${cpu_d} % of a core, host ${cpu_h} % of a core (debug build)"
python3 -I -c "import sys; sys.exit(0 if $cpu_h <= 2.0 else 1)" && ok "host <= 2 % of a core" || bad "host over 2 % of a core"

fi

echo "== done: $PASS passed, $FAIL failed"
[[ $FAIL == 0 ]]
