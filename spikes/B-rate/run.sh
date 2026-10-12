#!/usr/bin/env bash
# Spike B — sample rates and runtime retargeting of the smart filter. Private sinks only:
# omadsp_spike_out (48 kHz graph) and omadsp_spike_m44 (its own graph, node.force-rate = 44100).
# The hardware graph stays at 48 kHz; checked with pw-top.
set -u
source "$(dirname "$0")/../lib/common.sh"

M44=omadsp_spike_m44
sink44() {
  setsid bash -c '
    { printf "%s\n" "create-node adapter { factory.name = support.null-audio-sink node.name = $0 node.description = \"omadsp spike $0\" media.class = Audio/Sink audio.position = [ FL FR ] priority.session = 0 priority.driver = 0 node.force-rate = 44100 $1 }"
      exec sleep infinity; } | exec pw-cli' "$M44" "$NOSTATE" >/dev/null 2>&1 &
  echo $!
}
# rec_rate <out.wav> <sink> <rate> -> pid : hand-linked monitor recorder at a given rate
rec_rate() {
  local tag="omadsp_spike_rec_$RANDOM" pid i
  pw-record -P "{ node.autoconnect = false node.name = $tag $NOSTATE }" --format f32 --rate "$3" --channels 2 "$1" >/dev/null 2>&1 &
  pid=$!
  for i in $(seq 1 50); do
    pw-link "$2:monitor_FL" "$tag:input_FL" 2>/dev/null && pw-link "$2:monitor_FR" "$tag:input_FR" 2>/dev/null && break
    sleep 0.05
  done
  echo "$pid"
}
fmeta() { pw-metadata -n filters "$1" "$2" "$3" Spa:String:JSON >/dev/null; }
fmeta_del() { pw-metadata -n filters -d "$1" "$2" >/dev/null 2>&1; }
# delivered <wav> : seconds of samples above -100 dBFS (the LSP chain emits a tiny non-zero floor on silence)
delivered() { py -c 'import soundfile as sf, numpy as np, sys; x, sr = sf.read(sys.argv[1]); print(round(float(np.sum(np.max(np.abs(x), axis=1) > 1e-5)) / sr, 4))' "$1"; }

signals multi $RUN/multi44.wav --rate 44100 >/dev/null
signals multi $RUN/multi48.wav --rate 48000 >/dev/null
signals pink  $RUN/pink8.wav --secs 8 >/dev/null
signals sine  $RUN/sine1k-8.wav --freq 1000 --db -20 --secs 8 >/dev/null
BANDS=(peak:1000:6.0206:1.414 lowshelf:105:4:0.707 highshelf:8000:-3:0.707)
py "$LIB/rbj.py" curve $RUN/rbj44.json --rate 44100 "${BANDS[@]}"
py "$LIB/rbj.py" curve $RUN/rbj48.json --rate 48000 "${BANDS[@]}"

s=$(sink_start); s4=$(sink44); wait_node omadsp_spike_out >/dev/null; wait_node $M44 >/dev/null
mkhost $RUN/B.conf "$SPIKES/A-smart-filter/lsp-peq.conf.in"
h=$(start_host $RUN/B.conf $RUN/B.log); fid=$(wait_node omadsp_spike_eq) || { echo "host failed"; cat $RUN/B.log; }
# +6 dB bell @1k, +4 dB low shelf, -3 dB high shelf (linear gains), both channels
set_params $fid '"peq:gl_0" 2.0 "peq:gr_0" 2.0 "peq:gl_1" 1.584893 "peq:gr_1" 1.584893 "peq:gl_2" 0.707946 "peq:gr_2" 0.707946'
sleep 0.3

echo "== B1) LSP para_eq response vs RBJ at each rate"
r=$(rec_rate $RUN/B-r48.wav omadsp_spike_out 48000); sleep 0.3; play $RUN/multi48.wav omadsp_spike_out; sleep 0.3; rec_stop $r
echo -n "   48 kHz: "; measure response $RUN/multi48.wav $RUN/B-r48.wav --expect $RUN/rbj48.json --quiet
fmeta $fid filter.smart.target "{ \"node.name\": \"$M44\" }"; sleep 0.5
r=$(rec_rate $RUN/B-r44.wav $M44 44100); sleep 0.3
p=$(play_bg $RUN/multi44.wav $M44); sleep 1.5
echo "   pw-top while playing at 44.1k (QUANT RATE ... NAME):"; pw-top -b -n 3 2>/dev/null | grep -E "omadsp_spike_(m44|eq|eq_out)$|alsa_output.*analog" | tail -4 | awk '{print "     ", $1, $3, $4, $NF}'
wait $p 2>/dev/null; while kill -0 $p 2>/dev/null; do sleep 0.1; done; sleep 0.3; rec_stop $r
echo -n "   44.1 kHz: "; measure response $RUN/multi44.wav $RUN/B-r44.wav --expect $RUN/rbj44.json --quiet
fmeta_del $fid filter.smart.target; sleep 0.5

echo; echo "== B2) retarget 48k <-> 44.1k every 1 s while playing 8 s of pink"
ra=$(rec_rate $RUN/B-sw48.wav omadsp_spike_out 48000); rb=$(rec_rate $RUN/B-sw44.wav $M44 44100); sleep 0.3
p=$(play_bg $RUN/pink8.wav omadsp_spike_eq); sleep 1.3   # linked to the filter, as apps are when a filter is default-following
for i in 1 2 3 4 5 6; do
  if (( i % 2 )); then fmeta $fid filter.smart.target "{ \"node.name\": \"$M44\" }"; else fmeta_del $fid filter.smart.target; fi
  sleep 1
done
while kill -0 $p 2>/dev/null; do sleep 0.1; done; sleep 0.3; rec_stop $ra; rec_stop $rb
d48=$(delivered $RUN/B-sw48.wav); d44=$(delivered $RUN/B-sw44.wav)
echo "   delivered: 48k sink ${d48}s + 44.1k sink ${d44}s of 8.0 s played -> lost $(python3 -I -c "print(round(8.0-$d48-$d44,3))") s over 6 switches"
kill -0 $h 2>/dev/null && echo "   host alive" || echo "   HOST DIED"
grep -iE "error|warn" $RUN/B.log | head -5

echo; echo "== B3) filter.smart.disabled toggled every 0.5 s (streams relinked filter <-> sink), 1 kHz sine"
set_params $fid '"peq:gl_0" 1.0 "peq:gr_0" 1.0 "peq:gl_1" 1.0 "peq:gr_1" 1.0 "peq:gl_2" 1.0 "peq:gr_2" 1.0'
r=$(rec_rate $RUN/B-dis.wav omadsp_spike_out 48000); sleep 0.3
p=$(play_bg $RUN/sine1k-8.wav omadsp_spike_out); t0=$(date +%s%N); sleep 1; sw=""
for i in $(seq 1 10); do
  sw+=" $(python3 -I -c "print(round(($(date +%s%N)-$t0)/1e9,2))")"
  if (( i % 2 )); then fmeta $fid filter.smart.disabled true; else fmeta $fid filter.smart.disabled false; fi
  sleep 0.5
done
fmeta_del $fid filter.smart.disabled
while kill -0 $p 2>/dev/null; do sleep 0.1; done; sleep 0.3; rec_stop $r
echo "   toggles at (s from play start):$sw"
measure gaps $RUN/B-dis.wav --min-ms 0.5; measure clicks $RUN/B-dis.wav --freq 1000 --top 12

stop_host $h; sink_stop $s; sink_stop $s4; sleep 0.4
check_clean
