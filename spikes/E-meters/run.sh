#!/usr/bin/env bash
# Spike E — meters: where can an overload LED get its signal?
#  E1  are the LSP limiter's meter outputs (grlm/ilm/olm/sclm) visible in the filter's Props?
#  E2  a daemon-side tap: a capture stream hand-linked to the filter's playback-node output ports
#      (post-chain), next to the filter sink's monitor (pre-chain)
#  E3  Quickshell PwNodePeakMonitor (offscreen) on the device stand-in, the filter sink and the filter output
# Chain: bell 1 kHz +6 dB -> LSP limiter at -1 dBFS. A -3 dBFS tone is +3 dBFS after the EQ (limited);
# a -12 dBFS tone is -6 dBFS (not limited). Private sink only.
set -u
source "$(dirname "$0")/../lib/common.sh"
HERE="$SPIKES/E-meters"
CEIL=-1.0

baseline
md5sum ~/.local/state/wireplumber/stream-properties ~/.local/state/wireplumber/default-nodes >$RUN/E-wpstate.before
mkhost -D TH=0.891 $RUN/E.conf "$HERE/meter.conf.in"
signals sine $RUN/E-hot.wav --freq 1000 --db -3 --secs 4 >/dev/null
signals sine $RUN/E-safe.wav --freq 1000 --db -12 --secs 4 >/dev/null

# tap_start <out.wav> <node> <port-prefix> : hand-linked recorder on <node>:<prefix>_FL/_FR
tap_start() {
  local tag="omadsp_spike_rec_$RANDOM" pid i
  pw-record -P "{ node.autoconnect = false node.name = $tag $NOSTATE }" --format f32 --rate 48000 --channels 2 "$1" >/dev/null 2>&1 &
  pid=$!
  for i in $(seq 1 50); do
    pw-link "$2:$3_FL" "$tag:input_FL" 2>/dev/null && pw-link "$2:$3_FR" "$tag:input_FR" 2>/dev/null && break
    sleep 0.05
  done
  echo "$pid"
}
# qs_links : what each Quickshell peak stream targets, and what is actually linked into it
qs_links() {
  pw-dump >$RUN/E-dump.json 2>/dev/null
  py - $RUN/E-dump.json <<'PYEOF'
import json, sys
objs = json.load(open(sys.argv[1]))
nodes = {o["id"]: o["info"]["props"] for o in objs if o.get("type") == "PipeWire:Interface:Node"}
ports = {o["id"]: o["info"]["props"] for o in objs if o.get("type") == "PipeWire:Interface:Port"}
by_serial = {str(p.get("object.serial")): p.get("node.name") for p in nodes.values()}
links = {}
for o in objs:
    if o.get("type") != "PipeWire:Interface:Link": continue
    i = o["info"]
    links.setdefault(i["input-node-id"], set()).add(
        "%s:%s" % (nodes.get(i["output-node-id"], {}).get("node.name"), ports.get(i["output-port-id"], {}).get("port.name")))
for nid, n in nodes.items():
    if n.get("node.name") == "quickshell-peak-monitor":
        tgt = by_serial.get(str(n.get("target.object")), n.get("target.object"))
        print("      monitor on %-22s <- %s" % (tgt, ", ".join(sorted(links.get(nid, []))) or "(nothing linked)"))
PYEOF
}
now_ms() { date +%s%3N; }

s=$(sink_start); wait_node omadsp_spike_out >/dev/null
h=$(start_host $RUN/E.conf $RUN/E.log); wait_node omadsp_spike_eq_out >/dev/null; sleep 0.5

echo "== E1) filter Props while the limiter works (hot tone)"
p=$(play_bg $RUN/E-hot.wav omadsp_spike_out); sleep 1
for n in omadsp_spike_eq omadsp_spike_eq_out; do
  id=$(node_id $n)
  keys=$(pw-cli e "$id" Props 2>/dev/null | grep -oE '"(peq|limiter):[A-Za-z0-9_]+"' | tr -d '"' | sort -u)
  echo "   $n: $(echo "$keys" | grep -c .) controls in Props; limiter ones: $(echo "$keys" | grep '^limiter:' | sed 's/limiter://' | tr '\n' ' ')"
  echo "      meter outputs present: $(echo "$keys" | grep -E 'limiter:(grlm|ilm|olm|sclm)_' | tr '\n' ' ' || true)"
done
kill $p 2>/dev/null; wait $p 2>/dev/null
pw-link -o 2>/dev/null | grep -q '^omadsp_spike_eq:monitor_FL$' && PRE=1 || PRE=0
echo "   filter sink has monitor ports: $PRE"

echo; echo "== E3 setup) Quickshell offscreen with PwNodePeakMonitor on 3 nodes"
E_NODES=omadsp_spike_out,omadsp_spike_eq,omadsp_spike_eq_out QT_QPA_PLATFORM=offscreen QT_QUICK_BACKEND=software \
  quickshell -p "$HERE/qs" >$RUN/E-qs.log 2>&1 &
q=$!
sleep 3
echo "   Quickshell peak streams (target <- linked source ports):"; qs_links

for case in hot safe; do
  echo; echo "== E2/E3) $case tone ($( [[ $case == hot ]] && echo '-3 dBFS in, +3 dBFS after EQ, limited to -1' || echo '-12 dBFS in, -6 after EQ, not limited'))"
  rpost=$(tap_start $RUN/E-$case-post.wav omadsp_spike_eq_out output)
  ((PRE)) && rpre=$(tap_start $RUN/E-$case-pre.wav omadsp_spike_eq monitor)
  sleep 0.3; t0=$(now_ms)
  if [[ $case == hot ]]; then
    p=$(play_bg $RUN/E-$case.wav omadsp_spike_out); sleep 1
    echo "   Quickshell peak streams while playing:"; qs_links
    while kill -0 $p 2>/dev/null; do sleep 0.05; done
  else
    play $RUN/E-$case.wav omadsp_spike_out
  fi
  t1=$(now_ms); sleep 0.3
  rec_stop $rpost; ((PRE)) && rec_stop $rpre
  echo "   daemon tap, filter output (post-chain): $(measure peak $RUN/E-$case-post.wav --ceil $CEIL)"
  ((PRE)) && echo "   filter sink monitor (pre-chain):        $(measure peak $RUN/E-$case-pre.wav)"
  echo "   Quickshell PwNodePeakMonitor, loudest 250 ms window while playing (dBFS / node found / updates per window):"
  py - $RUN/E-qs.log $((t0 + 500)) $t1 <<'PYEOF'
import sys, re
log, a, b = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
best, ups = {}, {}
for line in open(log, errors="replace"):
    m = re.search(r"QSMETER (\d+) (.*)", line)
    if not m or not (a <= int(m.group(1)) <= b): continue
    for item in m.group(2).split():
        name, rest = item.split("=", 1)
        d, found, n = rest.split("/")
        v = float("-inf") if d == "-inf" else float(d)
        best[name] = max(best.get(name, float("-inf")), v)
        ups.setdefault(name, []).append(int(n))
for name in best:
    print(f"      {name:22} {best[name]:7.2f}   updates/250ms: {min(ups[name])}-{max(ups[name])}")
PYEOF
done

kill $q 2>/dev/null; wait $q 2>/dev/null

echo; echo "== E4) does the post-chain sample peak still sit at the ceiling in true-peak mode? (pink noise)"
signals pink $RUN/E-pink-hot.wav --db -12 --secs 4 >/dev/null
signals pink $RUN/E-pink-safe.wav --db -30 --secs 4 >/dev/null
cid=$(node_id omadsp_spike_eq)
for ovs in 0 22; do
  set_params $cid "\"limiter:ovs\" $ovs"; sleep 0.5
  for sig in pink-hot pink-safe; do
    rpost=$(tap_start $RUN/E-$sig-$ovs-post.wav omadsp_spike_eq_out output)
    rpre=$(tap_start $RUN/E-$sig-$ovs-pre.wav omadsp_spike_eq monitor)
    sleep 0.3; play $RUN/E-$sig.wav omadsp_spike_out; sleep 0.3
    rec_stop $rpost; rec_stop $rpre
    echo "   ovs=$ovs $sig: post $(measure peak $RUN/E-$sig-$ovs-post.wav --ceil $CEIL)   pre $(measure peak $RUN/E-$sig-$ovs-pre.wav)"
  done
done
grep -iE "warn|error|crit" $RUN/E-qs.log | grep -v QSMETER | head -5
pgrep -af "quickshell.*crash" | grep -v pgrep || true
stop_host $h; sink_stop $s; sleep 0.4
check_clean
md5sum ~/.local/state/wireplumber/stream-properties ~/.local/state/wireplumber/default-nodes | diff - $RUN/E-wpstate.before >/dev/null \
  && echo "WirePlumber state files unchanged" || echo "WirePlumber state files CHANGED"
