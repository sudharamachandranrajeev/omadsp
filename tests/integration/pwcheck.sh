#!/usr/bin/env bash
# Phase 1 step 2: does pipewire-rs cover what omadspd needs, and which preamp mechanism is click-free?
# Private test sink only (omadsp_spike_out); nothing reaches the speakers or the default device.
set -uo pipefail  # no -e: common.sh greps may legitimately match nothing
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=../../spikes/lib/common.sh
source "$ROOT/spikes/lib/common.sh"
OUT="$RUN/pwcheck"
mkdir -p "$OUT"
cargo build -q --manifest-path "$ROOT/Cargo.toml" -p omadspd --example pwcheck
PWC="$ROOT/target/debug/examples/pwcheck"

baseline
SINK="$(sink_start)"
wait_node omadsp_spike_out >/dev/null
mkhost "$OUT/host.conf" "$ROOT/tests/integration/pwcheck.conf.in"
HOST="$(start_host "$OUT/host.conf" "$OUT/host.log")"
PLAY=""
cleanup() {
  [[ -n $PLAY ]] && kill "$PLAY" 2>/dev/null
  stop_host "$HOST"
  sink_stop "$SINK"
  sleep 0.3
  check_clean || true
}
trap cleanup EXIT
wait_node omadsp_test >/dev/null
wait_node omadsp_test_out >/dev/null

signals sine "$OUT/tone.wav" --freq 1000 --db -20 --secs 120 --lead 0
PLAY="$(play_bg_relink "$OUT/tone.wav" omadsp_spike_out)"
sleep 1

echo "== 1. metadata and links"
"$PWC" info omadsp_test omadsp_spike_out | grep -E "default.audio.sink|configured.audio.sink|links into|filters" || true

echo "== 2. filters metadata: filter.smart.disabled"
"$PWC" smart omadsp_test off
sleep 0.5
"$PWC" info omadsp_spike_out | grep "links into"
"$PWC" smart omadsp_test on
sleep 0.5
"$PWC" info omadsp_spike_out | grep "links into"

# rec <name> : record the sink's monitor (hand-linked) until rec_end
rec() { REC="$(rec_start_raw "$OUT/$1.wav" omadsp_spike_out)"; sleep 0.6; }
rec_end() { sleep 0.2; rec_stop "$REC"; }
lvl() { measure level "$OUT/$1.wav" --freq 1000 "${@:2}" | python3 -I -c 'import json,sys; print(list(json.load(sys.stdin).values())[0][0])'; }

echo "== 3. Props params: eq:gl_0 0 dB -> +6.02 dB"
rec params
"$PWC" set omadsp_test eq:gl_0=2.0 eq:gr_0=2.0
sleep 0.8
"$PWC" set omadsp_test eq:gl_0=1.0 eq:gr_0=1.0
sleep 0.5
rec_end
echo "RESULT params before=$(lvl params --start 0.1 --len 0.4) during=$(lvl params --start 0.9 --len 0.4) after=$(lvl params --tail 0.3)"

echo "== 4. in-process tap, passive links"
"$PWC" tap omadsp_test_out 1

echo "== 5. preamp candidates: -12..0 dB triangle, 0.5 dB per change, then hold 0 dB"
# name node target period_ms [ramp_ms step]
run_sweep() {
  local name=$1 node=$2 target=$3 period=$4
  shift 4
  "$PWC" volume omadsp_test 1.0 >/dev/null
  "$PWC" volume omadsp_test_out 1.0 >/dev/null
  "$PWC" set omadsp_test pre:g_in=1.0 >/dev/null
  rec "$name"
  "$PWC" sweep "$node" "$target" -12 0 0.5 "$period" 3 "$@" >/dev/null
  rec_end
  local c
  c="$(measure clicks "$OUT/$name.wav" --freq 1000)"
  echo "RESULT $name start=$(lvl "$name" --start 0.1 --len 0.4) end=$(lvl "$name" --tail 0.5) clicks=$c"
}
run_sweep cap_step_20 omadsp_test volume 20
run_sweep cap_ramp15_20 omadsp_test volume 20 15 4
run_sweep cap_ramp15_50 omadsp_test volume 50 15 4
run_sweep cap_ramp40_50 omadsp_test volume 50 40 4
run_sweep out_ramp15_50 omadsp_test_out volume 50 15 4
run_sweep gin_step_20 omadsp_test pre:g_in 20
run_sweep gin_step_50 omadsp_test pre:g_in 50

echo "== host log (errors/warnings)"
grep -E -i "error|warn" "$OUT/host.log" | sort | uniq -c | head -20 || true
