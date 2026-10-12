#!/usr/bin/env bash
# Spike A′ — in-node filter graph (audioconvert.filter-graph.N) on private null sinks.
# Never touches the speakers. Usage: bash run.sh
set -u
source "$(dirname "$0")/../lib/common.sh"

# setgraph <node-id> <order> <graph-spa-json | "">  (empty string removes the graph)
setgraph() {
  local v
  v=$(python3 -I -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$3")
  pw-cli s "$1" Props "{ params = [ \"audioconvert.filter-graph.$2\" $v ] }" >/dev/null
}
# sink2 <name> <monitor.channel-volumes> -> pgid  (like sink_start, but choose monitor volume behaviour)
sink2() {
  setsid bash -c '
    { printf "%s\n" "create-node adapter { factory.name = support.null-audio-sink node.name = $0 node.description = \"omadsp spike $0\" media.class = Audio/Sink audio.position = [ FL FR ] priority.session = 0 priority.driver = 0 monitor.channel-volumes = $1 state.restore-props = false state.restore-target = false }"
      exec sleep infinity; } | exec pw-cli' "$1" "$2" >/dev/null 2>&1 &
  echo $!
}
# rec the monitor of $2 while playing $1 into $2
pr() { local r; r=$(rec_start_raw "$3" "$2"); sleep 0.3; play "$1" "$2"; sleep 0.3; rec_stop "$r"; }

[[ -f $RUN/sine1k-20.wav ]] || signals sine $RUN/sine1k-20.wav --freq 1000 --db -20 --secs 20 >/dev/null
[[ -f $RUN/sine1k-3.wav ]]  || signals sine $RUN/sine1k-3.wav  --freq 1000 --db -3  --secs 2 >/dev/null
signals sine $RUN/sine1k-20s.wav --freq 1000 --db -20 --secs 3 >/dev/null

M0=omadsp_spike_m0   # monitor.channel-volumes = false (like real ALSA sinks)
M1=omadsp_spike_m1   # plain test sink, monitor.channel-volumes = true
LB=omadsp_spike_lb   # pw-loopback capture node: an adapter Audio/Sink like a device sink, whose
                     # output we *can* record (lb -> lb_out -> M1). A sink's monitor is pre-graph (test 1),
                     # so graph effects are measured after the loopback, on M1's monitor.
s0=$(sink2 $M0 false); s1=$(sink2 $M1 false)
id0=$(wait_node $M0); id1=$(wait_node $M1)
pw-loopback -n omadsp-spike-lb \
  --capture-props="{ node.name = $LB media.class = Audio/Sink audio.position = [ FL FR ] priority.session = 0 $NOSTATE }" \
  --playback-props="{ node.name = ${LB}_out target.object = $M1 node.passive = true audio.position = [ FL FR ] $NOSTATE }" \
  >/dev/null 2>&1 & lbp=$!
idl=$(wait_node $LB); wait_node ${LB}_out >/dev/null; sleep 0.3
echo "sinks: $M0=$id0 $M1=$id1 loopback=$idl"
# play into the loopback sink, record M1's monitor (= loopback output, post-graph)
lr() { local r; r=$(rec_start_raw "$2" $M1); sleep 0.3; play "$1" $LB; sleep 0.4; rec_stop "$r"; }

MUTE='{ nodes = [ { type = builtin name = g label = linear control = { Mult = 0.0 } } ] }'
HALF='{ nodes = [ { type = builtin name = g label = linear control = { Mult = 0.5 } } ] }'
CLAMP='{ nodes = [ { type = builtin name = c label = clamp control = { Min = -0.5 Max = 0.5 } } ] }'

echo; echo "== 1) Mult=0 graph on $M0: is the monitor pre- or post-graph?"
setgraph $id0 0 "$MUTE"; sleep 0.2
pr $RUN/sine1k-20s.wav $M0 $RUN/A2-mute.wav; measure level $RUN/A2-mute.wav --freq 1000
setgraph $id0 0 ""

echo; echo "== 2) volume order on the loopback sink (loopback node volume): clamp ±0.5 graph, -3 dBFS tone, channel volume 0.5"
echo "   graph before volume -> peak 0.25 (-12 dB); volume before graph -> 0.354 (-9 dB)"
setgraph $idl 0 "$CLAMP"; pw-cli s $idl Props '{ channelVolumes = [ 0.5 0.5 ] }' >/dev/null; sleep 0.2
lr $RUN/sine1k-3.wav $RUN/A2-vol.wav; measure rms $RUN/A2-vol.wav
echo "   control: same but no graph"
setgraph $idl 0 ""; sleep 0.2
lr $RUN/sine1k-3.wav $RUN/A2-vol0.wav; measure rms $RUN/A2-vol0.wav
pw-cli s $idl Props '{ channelVolumes = [ 1.0 1.0 ] }' >/dev/null

echo; echo "== 3) live control key format (graph Mult=0.5 on the loopback sink, then set 1.0 / 0.25)"
setgraph $idl 0 "$HALF"; sleep 0.2
lr $RUN/sine1k-20s.wav $RUN/A2-k0.wav; echo -n "   initial 0.5:          "; measure level $RUN/A2-k0.wav --freq 1000
set_params $idl '"g:Mult" 1.0'; sleep 0.2
lr $RUN/sine1k-20s.wav $RUN/A2-k1.wav; echo -n "   'g:Mult' 1.0:         "; measure level $RUN/A2-k1.wav --freq 1000
set_params $idl '"0:g:Mult" 0.25'; sleep 0.2
lr $RUN/sine1k-20s.wav $RUN/A2-k2.wav; echo -n "   '0:g:Mult' 0.25:      "; measure level $RUN/A2-k2.wav --freq 1000
echo "   Props readback: $(pw-dump $idl | python3 -I -c 'import json,sys; o=json.load(sys.stdin)[0]; print([p for p in o["info"]["params"].get("Props",[]) if "params" in p and any("g:" in str(x) for x in p["params"])][:1])')"
setgraph $idl 0 ""

echo; echo "== 4) LV2 limiter_stereo inside the node (th 0.25 = -12 dBFS, -3 dBFS tone)"
LIM='{ nodes = [ { type = lv2 name = lim plugin = "http://lsp-plug.in/plugins/lv2/limiter_stereo" control = { alr = 0 boost = 0 th = 0.25 lk = 5.0 ovs = 0 } } ] inputs = [ "lim:in_l" "lim:in_r" ] outputs = [ "lim:out_l" "lim:out_r" ] }'
setgraph $idl 0 "$LIM"; sleep 0.3
lr $RUN/sine1k-3.wav $RUN/A2-lim.wav; echo -n "   limited:      "; measure level $RUN/A2-lim.wav --freq 1000
set_params $idl '"lim:th" 1.0'; sleep 0.3
lr $RUN/sine1k-3.wav $RUN/A2-lim2.wav; echo -n "   th -> 1.0:    "; measure level $RUN/A2-lim2.wav --freq 1000

echo; echo "== 5) graph replace while playing (same order, 10 swaps at 2/s): gaps and clicks"
r=$(rec_start_raw $RUN/A2-swap.wav $M1); sleep 0.3
p=$(play_bg $RUN/sine1k-20.wav $LB)
t0=$(date +%s%N); sleep 1; sw=""
for i in $(seq 1 10); do
  sw+=" $(python3 -I -c "print(round(($(date +%s%N)-$t0)/1e9,2))")"
  if (( i % 2 )); then setgraph $idl 0 "$HALF"; else setgraph $idl 0 '{ nodes = [ { type = builtin name = g label = linear control = { Mult = 0.5 } } ] }'; fi
  sleep 0.5
done
kill $p 2>/dev/null; sleep 0.3; rec_stop $r
pgrep -x pw-cat >/dev/null && echo "WARNING: pw-cat still running"
echo "   swaps at (s from play start):$sw"
measure gaps $RUN/A2-swap.wav --min-ms 0.5; measure clicks $RUN/A2-swap.wav --freq 1000 --top 8

echo; echo "== 6) audioconvert.filter-graph.disable = true with a graph loaded (HALF)"
setgraph $idl 0 "$HALF"; sleep 0.2
set_params $idl '"audioconvert.filter-graph.disable" true'; sleep 0.2
lr $RUN/sine1k-20s.wav $RUN/A2-dis.wav; echo -n "   disabled=true: "; measure level $RUN/A2-dis.wav --freq 1000
setgraph $idl 1 "$HALF"; sleep 0.2
lr $RUN/sine1k-20s.wav $RUN/A2-dis2.wav; echo -n "   + load order 1 while disabled: "; measure level $RUN/A2-dis2.wav --freq 1000
set_params $idl '"audioconvert.filter-graph.disable" false'
setgraph $idl 0 ""; setgraph $idl 1 ""; sleep 0.2
lr $RUN/pink.wav $RUN/A2-none.wav; echo -n "   all removed, null vs source: "; measure null $RUN/pink.wav $RUN/A2-none.wav

echo; echo "== 7) setter gone: pw-cli exited after every set above; graph from (5) stayed active -> see 6"
kill $lbp; wait $lbp 2>/dev/null; sink_stop $s0; sink_stop $s1; sleep 0.3
check_clean
