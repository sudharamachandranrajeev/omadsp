#!/usr/bin/env bash
# Phase 1 step 6: the engine end to end — startup, on/off, live changes, A/B, preset switch, drags,
# host crash, daemon kill. Private test sink only (omadsp_spike_out); nothing reaches the speakers
# or the default device.
set -uo pipefail  # no -e: greps may legitimately match nothing
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=../../spikes/lib/common.sh
source "$ROOT/spikes/lib/common.sh"
OUT="$RUN/engine"
rm -rf "$OUT"
mkdir -p "$OUT/rt"
cargo build -q --manifest-path "$ROOT/Cargo.toml" -p omadspd --example enginecheck
EC="$ROOT/target/debug/examples/enginecheck"

baseline
SINK="$(sink_start)"
wait_node omadsp_spike_out >/dev/null
PLAY="" EPID=""
cleanup() {
  exec 3>&- 2>/dev/null
  [[ -n $EPID ]] && kill "$EPID" 2>/dev/null && wait "$EPID" 2>/dev/null
  [[ -n $PLAY ]] && kill "$PLAY" 2>/dev/null
  sink_stop "$SINK"
  sleep 0.3
  check_clean || true
}
trap cleanup EXIT

signals sine "$OUT/tone.wav" --freq 1000 --db -20 --secs 120 --lead 0
PLAY="$(play_bg_relink "$OUT/tone.wav" omadsp_spike_out)"

mkfifo "$OUT/cmd"
"$EC" --name omadsp_test --target omadsp_spike_out --runtime-dir "$OUT/rt" -v <"$OUT/cmd" >"$OUT/engine.out" 2>"$OUT/engine.log" &
EPID=$!
exec 3>"$OUT/cmd"
send() { echo "$*" >&3; }
# wait_for <regex> [tenths]
wait_for() {
  local i
  for i in $(seq 1 "${2:-50}"); do grep -Eq "$1" "$OUT/engine.out" && return 0; sleep 0.1; done
  echo "TIMEOUT waiting for: $1"; return 1
}
host_pid() { pgrep -f "pipewire -c $OUT/rt/host.conf"; }
calc() { awk "BEGIN { printf \"%.3f\", $* }"; }
lvl() { measure level "$1" --freq 1000 --start "$2" --len "${3:-0.4}" | python3 -I -c 'import json,sys; print(list(json.load(sys.stdin).values())[0][0])'; }

echo "== 1. startup: DSP off, filter up but not in the path"
wait_for "filter up" 50 >/dev/null || grep -E "filter up" "$OUT/engine.log"
wait_for "STATUS engine=Off" && grep -c . /dev/null >/dev/null
sleep 0.5
echo "RESULT links_into_sink=$(links_of omadsp_spike_out | grep -c "omadsp_test_out") (0 = filter not linked while off)"

# Scripted session: one command per 1.2 s step; levels measured 0.45–0.85 s into each step.
# session <name> <cmd>... : records the sink monitor, prints "RESULT step cmd level expected"
MARKS="$OUT/marks"
session() {
  local name=$1; shift
  : >"$MARKS"
  local rec t0
  rec="$(rec_start_raw "$OUT/$name.wav" omadsp_spike_out)"
  t0=$(date +%s.%N)
  sleep 1.2
  local c
  for c in "$@"; do
    echo "$(calc "$(date +%s.%N) - $t0") ${c}" >>"$MARKS"
    send "${c%%=*}"
    sleep 1.2
  done
  rec_stop "$rec"
}
report() {
  local wav="$OUT/$1.wav"
  echo "RESULT $1 before: $(lvl "$wav" 0.5)"
  while read -r t c; do
    local cmd="${c%%=*}" exp="${c#*=}"
    printf 'RESULT %-12s %-8s level=%s\n' "$cmd" "exp=$exp" "$(lvl "$wav" "$(calc "$t + 0.45")")"
  done <"$MARKS"
  # clicks: attribute every 5 ms window over -60 dBc to the command just before it
  measure clicks "$wav" --freq 1000 --top 40 >"$OUT/$1.clicks"
  python3 -I - "$OUT/$1.clicks" "$MARKS" <<'PY'
import json, sys
c = json.load(open(sys.argv[1]))
marks = [(float(l.split()[0]), l.split(None, 1)[1].strip().split("=")[0]) for l in open(sys.argv[2]) if l.strip()]
by = {}
for t, d in c.get("top_s_dBc", []):
    # top_s_dBc times are from signal onset; the recording starts with the tone, so onset ~ 0
    prev = [m for m in marks if m[0] <= t + 0.05]
    k = prev[-1][1] if prev else "(before first command)"
    by.setdefault(k, []).append(d)
print(f"RESULT clicks residual_peak={c['residual_peak_db']} median={c['residual_median_db']} windows_over_-60dBc={c['windows_over_-60dBc']}")
for k, v in by.items():
    print(f"RESULT clicks after {k!r}: {len(v)} windows, worst {max(v)} dBc")
PY
}

echo "== 2. on, live changes, A/B, preset switch, off (1 kHz at -20 dBFS; expected levels in dBFS)"
session s2 "on=-20" "band 5 6=-20 (auto -6)" "auto 0=-14" "ab 1=-20" "ab 0=-14" "preamp -3=-17" \
  "preset Flat=-20" "band 5 -6=-26" "off=-20"
report s2
grep -E "STATUS engine=(Running|Off|Starting)" "$OUT/engine.out" | uniq | tail -4 | sed 's/^/  /'

echo "== 3. drags: band 5 and the preamp moved every 50 ms for 3 s, DSP on"
send on
wait_for "engine=Running" 30 >/dev/null
sleep 0.5
rec="$(rec_start_raw "$OUT/s3.wav" omadsp_spike_out)"
sleep 0.5
for i in $(seq 0 59); do
  g=$(( (i % 24) < 12 ? (i % 24) - 6 : 18 - (i % 24) ))
  send "band 5 $g"
  sleep 0.05
done
send "auto 0"
for i in $(seq 0 59); do
  g=$(( (i % 24) < 12 ? -(i % 24) : (i % 24) - 24 ))
  send "preamp $(calc "$g / 2")"
  sleep 0.05
done
send "band 5 0"; send "preamp 0"; send "auto 1"
sleep 1
rec_stop "$rec"
echo "RESULT drags clicks $(measure clicks "$OUT/s3.wav" --freq 1000)"
echo "RESULT drags end level=$(measure level "$OUT/s3.wav" --freq 1000 --tail 0.3 | python3 -I -c 'import json,sys; print(list(json.load(sys.stdin).values())[0][0])') (exp -20)"

echo "== 4. the host crashes (SIGKILL) while on: audio falls through, the engine restarts it"
send "band 5 -6"
sleep 1
rec="$(rec_start_raw "$OUT/s4.wav" omadsp_spike_out)"
sleep 1
HP="$(host_pid)"
kill -KILL "$HP"
sleep 3
rec_stop "$rec"
echo "RESULT old host=$HP new host=$(host_pid)"
echo "RESULT crash levels: before=$(lvl "$OUT/s4.wav" 0.4) gap+0.15s=$(lvl "$OUT/s4.wav" 1.15 0.1) end=$(lvl "$OUT/s4.wav" 3.4 0.4) (exp -26, -20 or silence, -26)"
grep -E "exited|restarting|filter up" "$OUT/engine.log" | sed 's/^.*omadspd::engine[^ ]* //' | tail -3 | sed 's/^/  /'
measure gaps "$OUT/s4.wav" --min-ms 5 | sed 's/^/RESULT crash gaps /'

echo "== 5. the daemon is killed (SIGKILL): PDEATHSIG stops the host, streams fall through"
HP="$(host_pid)"
kill -KILL "$EPID"; wait "$EPID" 2>/dev/null; EPID=""
sleep 0.5
if kill -0 "$HP" 2>/dev/null; then echo "RESULT host still running (BAD)"; else echo "RESULT host gone"; fi
echo "RESULT links into sink: $(links_of omadsp_spike_out | grep -o 'pw-cat[^ ]*\|omadsp_test_out[^ ]*' | sort -u | tr '\n' ' ')"
echo "RESULT filters metadata now: $(pw-metadata -n filters 0 2>/dev/null | grep -v '^Found' | tr '\n' ' ')"
pw-metadata -n filters 2>/dev/null | grep -i "smart" | sed 's/^/  /'
