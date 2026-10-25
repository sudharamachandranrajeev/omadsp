#!/usr/bin/env bash
# Phase 1 step 7: omadspd + the omadsp CLI end to end — socket, commands, errors, events, presets,
# persistence and restart. Private test sink only (omadsp_spike_out); config and state go to a
# scratch directory, never ~/.config or ~/.local/state.
set -uo pipefail  # no -e: failing commands are part of the test
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=../../spikes/lib/common.sh
source "$ROOT/spikes/lib/common.sh"
OUT="$RUN/ipc"
rm -rf "$OUT"
mkdir -p "$OUT/rt"
cargo build -q --manifest-path "$ROOT/Cargo.toml" -p omadspd -p omadsp
D="$ROOT/target/debug/omadspd"
C="$ROOT/target/debug/omadsp"
export OMADSP_SOCKET="$OUT/rt/omadsp.sock"
CFG="$OUT/config" STATE="$OUT/state"

baseline
SINK="$(sink_start)"
wait_node omadsp_spike_out >/dev/null
PLAY="" DPID="" WPID=""
cleanup() {
  [[ -n $WPID ]] && kill "$WPID" 2>/dev/null
  [[ -n $DPID ]] && kill "$DPID" 2>/dev/null && wait "$DPID" 2>/dev/null
  [[ -n $PLAY ]] && kill "$PLAY" 2>/dev/null
  sink_stop "$SINK"
  sleep 0.3
  check_clean || true
}
trap cleanup EXIT

PASS=0 FAIL=0
ok() { echo "PASS $*"; PASS=$((PASS + 1)); }
bad() { echo "FAIL $*"; FAIL=$((FAIL + 1)); }
# expect <label> <python expr over s (status JSON)>
expect() {
  local v
  v="$("$C" status --json | python3 -I -c "import json,sys; s=json.load(sys.stdin); print(bool($2))")"
  [[ $v == True ]] && ok "$1" || bad "$1 ($2)"
}
# code <label> <expected exit> <cmd...>
code() {
  local want=$2 label=$1; shift 2
  "$@" >/dev/null 2>"$OUT/last.err"; local got=$?
  [[ $got == "$want" ]] && ok "$label (exit $got)" || bad "$label: exit $got, wanted $want: $(cat "$OUT/last.err")"
}
start_daemon() {
  "$D" --name omadsp_test --target omadsp_spike_out --runtime-dir "$OUT/rt" --config-dir "$CFG" --state-dir "$STATE" -v \
    >>"$OUT/daemon.log" 2>&1 &
  DPID=$!
  local i
  for i in $(seq 1 50); do [[ -S $OMADSP_SOCKET ]] && "$C" status >/dev/null 2>&1 && return 0; sleep 0.1; done
  echo "daemon did not come up"; tail "$OUT/daemon.log"; exit 1
}
wait_engine() {
  local i
  for i in $(seq 1 50); do
    "$C" status --json | grep -q "\"engine\": \"$1\"" && return 0
    sleep 0.1
  done
  return 1
}
lvl() { measure level "$1" --freq 1000 --start 0.3 --len 0.5 | python3 -I -c 'import json,sys; print(list(json.load(sys.stdin).values())[0][0])'; }
level_now() {
  local rec
  rec="$(rec_start_raw "$OUT/$1.wav" omadsp_spike_out)"
  sleep 1
  rec_stop "$rec"
  lvl "$OUT/$1.wav"
}
near() { python3 -I -c "import sys; sys.exit(0 if abs($1 - ($2)) <= ${3:-0.3} else 1)"; }

echo "== 1. first run: off, Flat, nothing written"
code "no daemon -> exit 2" 2 "$C" status
start_daemon
expect "starts off" "not s['enabled'] and s['engine'] == 'off'"
expect "starts on Flat, clean" "s['preset'] == 'Flat' and not s['dirty']"
[[ ! -e $CFG && ! -e $STATE ]] && ok "no config or state files yet" || bad "files written at startup"
code "second daemon refuses the socket" 1 "$D" --name omadsp_test2 --target omadsp_spike_out --runtime-dir "$OUT/rt" \
  --config-dir "$CFG" --state-dir "$STATE"

echo "== 2. audible changes through the CLI (1 kHz at -20 dBFS)"
signals sine "$OUT/tone.wav" --freq 1000 --db -20 --secs 120 --lead 0
PLAY="$(play_bg_relink "$OUT/tone.wav" omadsp_spike_out)"
sleep 0.5
"$C" on
wait_engine running && ok "engine running after 'on'" || bad "engine never ran"
sleep 0.5
L=$(level_now on); near "$L" -20 && ok "on, flat: $L dBFS" || bad "on, flat: $L (exp -20)"
"$C" band 6 6
"$C" preamp --manual
sleep 0.3
L=$(level_now band); near "$L" -14 0.5 && ok "band 6 +6, manual preamp: $L dBFS" || bad "band 6 +6: $L (exp -14)"
"$C" preamp -3
sleep 0.3
L=$(level_now preamp); near "$L" -17 0.5 && ok "preamp -3: $L dBFS" || bad "preamp -3: $L (exp -17)"
"$C" ab on
sleep 0.3
L=$(level_now ab); near "$L" -20 && ok "A/B held: $L dBFS" || bad "A/B: $L (exp -20)"
"$C" ab
expect "ab toggles off" "not s['ab']"

echo "== 3. errors"
code "band out of range" 1 "$C" band 6 20
code "no band 99" 1 "$C" band 99 1
code "graphic band has no freq" 1 "$C" band 1 --freq 100
code "preamp out of range" 1 "$C" preamp 30
code "unknown preset" 1 "$C" preset apply Nope
code "built-in can't be saved" 1 "$C" preset save Flat
code "later-phase request" 1 "$C" raw stereo.set '{"width":1}'
grep -q "not_implemented" "$OUT/last.err" && ok "later phase says not_implemented" || bad "not_implemented: $(cat "$OUT/last.err")"
python3 -I - "$OMADSP_SOCKET" >"$OUT/raw.out" <<'PY'
import json, socket, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); f = s.makefile("rwb")
for line in [b"garbage\n", b'{"v":2,"id":1,"type":"state.get"}\n', b'{"v":1,"id":"x","type":"band.set","payload":{"index":0,"gian":1}}\n',
             b'{"v":1,"id":"y","type":"nope"}\n', b'{"v":1,"id":"z","type":"state.get"}\n']:
    f.write(line); f.flush()
    while True:
        m = json.loads(f.readline())
        if "event" not in m: break
    print(m.get("id"), m["ok"], (m.get("error") or {}).get("code"))
PY
diff <(printf 'None False bad_request\n1 False bad_request\nx False bad_request\ny False unknown_type\nz True None\n') "$OUT/raw.out" \
  && ok "bad lines get errors and the connection survives" || bad "raw: $(cat "$OUT/raw.out")"

echo "== 4. events"
"$C" watch >"$OUT/watch.out" &
WPID=$!
sleep 0.3
"$C" band 2 1.5
"$C" off
"$C" on
sleep 0.3
kill "$WPID"; WPID=""
python3 -I - "$OUT/watch.out" <<'PY'
import json, sys
ev = [json.loads(l) for l in open(sys.argv[1])]
names = [e["event"] for e in ev]
print("RESULT events:", {n: names.count(n) for n in set(names)})
assert names[:2] == ["state.changed", "curve.updated"], names[:2]
curves = [e["payload"] for e in ev if e["event"] == "curve.updated"]
assert len(curves[-1]["left"]) == 256 and curves[-1]["right"] is None
assert any(not e["payload"]["enabled"] for e in ev if e["event"] == "state.changed")
PY
[[ $? == 0 ]] && ok "watch: snapshot + curve on connect, changes after" || bad "events"

echo "== 5. presets"
"$C" preset save Test One && ok "saved 'Test One'" || bad "save"
F="$CFG/presets/test-one.json"
[[ -f $F ]] && head -2 "$F" | grep -q "Managed by omadsp" && ok "preset file has the marker" || bad "preset file $F"
expect "current is Test One, clean" "s['preset'] == 'Test One' and not s['dirty']"
"$C" preset next; expect "next wraps to Flat" "s['preset'] == 'Flat'"
"$C" preset prev; expect "prev back to Test One" "s['preset'] == 'Test One' and s['working']['eq']['gains'][5] == 6"
"$C" preset Warm V; expect "apply by bare name" "s['preset'] == 'Warm V'"
"$C" preset rename "Test One" Mine
[[ -f $CFG/presets/mine.json && ! -e $F ]] && ok "rename moved the file" || bad "rename files: $(ls "$CFG/presets")"
"$C" preset lock Mine
code "locked can't be deleted" 1 "$C" preset delete Mine
"$C" preset unlock Mine
code "unlocked deletes" 0 "$C" preset delete Mine
"$C" preset list | grep -q Mine && bad "Mine still listed" || ok "Mine gone from the list"

echo "== 6. a burst: 300 band.set on one connection"
python3 -I - "$OMADSP_SOCKET" <<'PY'
import json, socket, sys, time
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); f = s.makefile("rwb")
t = time.time()
for i in range(300):
    f.write(json.dumps({"v": 1, "id": i, "type": "band.set", "payload": {"index": 3, "g": (i % 25) - 12}}).encode() + b"\n")
f.flush()
got = 0
while got < 300:
    m = json.loads(f.readline())
    if "event" in m: continue
    assert m["ok"], m
    got += 1
print(f"RESULT 300 responses in {time.time() - t:.2f}s")
PY
[[ $? == 0 ]] && ok "burst answered" || bad "burst"

echo "== 7. meter tap and overload LED"
has_tap() { [[ -n "$(node_id omadsp_test_meter)" ]]; }
has_tap && bad "tap exists before anyone subscribes" || ok "no tap while nobody subscribes"
"$C" preset Flat
"$C" preamp 0 --manual
"$C" status >"$OUT/meter-status.txt"
"$C" meter --json >"$OUT/meter-full.out" &
WPID=$!
"$C" meter --led --json >"$OUT/meter-led.out" &
LPID=$!
sleep 1.5
has_tap && ok "tap created on subscribe" || bad "no tap node"
python3 -I - "$OUT/meter-full.out" -20 false <<'PY'
import json, sys
m = [json.loads(l)["payload"] for l in open(sys.argv[1]) if '"meter"' in l]
want, hot = float(sys.argv[2]), sys.argv[3] == "true"
tail = m[-10:]
print(f"RESULT {len(m)} meter events, last peaks {tail[-1]['peak_db'] if tail else None}")
assert len(m) >= 15, "expected ~20 Hz"
assert all(abs(p - want) <= 0.5 for e in tail for p in e["peak_db"]), tail
assert all(e["overload"] == hot for e in tail)
PY
[[ $? == 0 ]] && ok "meter at -20 dBFS, LED off" || bad "cold meter readings (sink now: $(level_now meter-sink) dBFS)"
SKIP=$(wc -l <"$OUT/meter-full.out")
"$C" band 6 15
"$C" preamp 12
sleep 1.5
python3 -I - "$OUT/meter-full.out" "$SKIP" <<'PY'
import json, sys
m = [json.loads(l)["payload"] for l in open(sys.argv[1]).readlines()[int(sys.argv[2]):] if '"meter"' in l]
tail = m[-10:]
print(f"RESULT hot: last peaks {tail[-1]['peak_db'] if tail else None}")
assert tail and all(max(e["peak_db"]) <= -0.9 and max(e["peak_db"]) >= -1.5 for e in tail), tail
assert all(e["overload"] for e in tail)
PY
[[ $? == 0 ]] && ok "+7 dB into the limiter: peaks at the -1 dBTP ceiling, LED on" || bad "hot meter readings"
expect "snapshot shows overload" "s['overload']"
"$C" preset Flat
sleep 1
grep -q '"on":true' "$OUT/meter-led.out" && grep -q '"on":false' "$OUT/meter-led.out" && ok "led subscriber got overload on and off" \
  || bad "overload events: $(cat "$OUT/meter-led.out")"
grep -q '"event":"meter"' "$OUT/meter-led.out" && bad "led subscriber got meter events" || ok "led subscriber gets no meter events"
kill "$WPID" "$LPID"; WPID=""
sleep 0.5
has_tap && bad "tap still there after unsubscribing" || ok "tap removed when the last subscriber left"
expect "overload off without a tap" "not s['overload']"

echo "== 8. persistence and restart"
"$C" band 3 -4.5
"$C" tone --bass 3 --treble -2
sleep 1.5
[[ -f $STATE/state.json ]] && ok "state.json written within ~1 s" || bad "no state.json"
kill -TERM "$DPID"; wait "$DPID" 2>/dev/null; DPID=""
[[ ! -e $OMADSP_SOCKET ]] && ok "socket removed on SIGTERM" || bad "socket left behind"
python3 -I -c "import json,sys; s=json.load(open(sys.argv[1])); assert s['enabled'] and s['working']['eq']['gains'][2] == -4.5" "$STATE/state.json" \
  && ok "state.json holds the last values" || bad "state.json content"
start_daemon
expect "restart restores on + values" "s['enabled'] and s['working']['eq']['gains'][2] == -4.5 and s['working']['tone']['bass']['db'] == 3"
wait_engine running && ok "engine running again after restart" || bad "engine after restart"
"$C" off

echo "RESULT pass=$PASS fail=$FAIL"
[[ $FAIL == 0 ]]
