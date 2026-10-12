#!/usr/bin/env bash
# Spike F — spatial: (F1) PipeWire's HeSuVi example as shipped, (F2-F5) a sofa spatializer with SADIE II D2
# (assets/, git-ignored): stereo -> virtual speakers at +/-30 degrees. Private sink only.
set -u
source "$(dirname "$0")/../lib/common.sh"
HERE="$SPIKES/F-spatial"
SOFA="$HERE/assets/D2_48K_24bit_256tap_FIR_SOFA.sofa"
baseline

echo "== F1) HeSuVi 7.1 example (/usr/share/pipewire/filter-chain), own node names, as a smart filter"
py - /usr/share/pipewire/filter-chain/sink-virtual-surround-7.1-hesuvi.conf $RUN/F-hesuvi.frag <<'PYEOF'
import re, sys
t = open(sys.argv[1]).read()
i = t.index("{ name = libpipewire-module-filter-chain")
d = 0
for j in range(i, len(t)):
    d += {"{": 1, "}": -1}.get(t[j], 0)
    if d == 0: break
m = t[i:j + 1].replace("flags = [ nofail ]", "")
m = re.sub(r"capture\.props = \{.*?\n\s*\}", """capture.props = {
                node.name = "omadsp_spike_eq" media.class = Audio/Sink audio.channels = 8
                audio.position = [ FL FR FC LFE RL RR SL SR ] filter.smart = true filter.smart.name = "omadsp-spike"
                filter.smart.target = { node.name = "omadsp_spike_out" } state.restore-props = false state.restore-target = false
            }""", m, flags=re.S)
m = re.sub(r"playback\.props = \{.*?\n\s*\}", """playback.props = {
                node.name = "omadsp_spike_eq_out" node.passive = true audio.channels = 2 audio.position = [ FL FR ]
                state.restore-props = false state.restore-target = false
            }""", m, flags=re.S)
open(sys.argv[2], "w").write(m)
PYEOF
s=$(sink_start); wait_node omadsp_spike_out >/dev/null
mkhost $RUN/F-hesuvi.conf $RUN/F-hesuvi.frag
# the convolver sf_open()s the relative path "hrir_hesuvi/hrir.wav" from the host's working directory
signals sine $RUN/F-sine.wav --freq 1000 --db -12 --secs 1.5 >/dev/null
(cd $RUN && exec pipewire -c $RUN/F-hesuvi.conf >$RUN/F-hesuvi.log 2>&1) & hh=$!
wait_node omadsp_spike_eq_out >/dev/null && echo "   module loaded; filter nodes exist (plugins are only instantiated when audio starts)"
r=$(rec_start_raw $RUN/F-hesuvi.wav omadsp_spike_out); sleep 0.3
p=$(play_bg_relink $RUN/F-sine.wav omadsp_spike_out); sleep 0.8
echo "   while playing, the stream feeds: $(pw-link -l 2>/dev/null | grep -A1 '^pw-cat:output_FL' | tail -1 | tr -s ' ')"
while kill -0 $p 2>/dev/null; do sleep 0.05; done; sleep 0.3; rec_stop $r
echo "   delivered to the sink: $(measure peak $RUN/F-hesuvi.wav)"
kill -0 $hh 2>/dev/null && echo "   host still running" || echo "   host exited"
stop_host $hh
grep -vE "^\s*$" $RUN/F-hesuvi.log | sed 's/^/   | /' | head -8
echo "   any hrir_hesuvi dir or hrir.wav installed: $(find /usr/share /etc/pipewire ~/.config/pipewire -name 'hrir_hesuvi' -o -name 'hrir.wav' 2>/dev/null | head -3 | tr '\n' ' ' || true)"

echo; echo "== F2) sofa spatializer: load"
[[ -f $SOFA ]] || { echo "   missing $SOFA"; sink_stop $s; exit 1; }
mkhost -D SOFA="$SOFA" $RUN/F.conf "$HERE/sofa.conf.in"
t0=$(date +%s%N); h=$(PIPEWIRE_DEBUG=3 start_host $RUN/F.conf $RUN/F.log)
until pw-link -o 2>/dev/null | grep -q '^omadsp_spike_eq_out:output_FL$'; do sleep 0.01; kill -0 $h 2>/dev/null || break; done
echo "   spawn -> output port: $(( ($(date +%s%N) - t0) / 1000000 )) ms (SOFA file $(du -m "$SOFA" | cut -f1) MB)"
kill -0 $h 2>/dev/null || { echo "   host died:"; tail -5 $RUN/F.log; sink_stop $s; exit 1; }
sleep 0.5
echo "   host RSS: $(awk '/VmRSS/ {printf "%.1f MiB", $2/1024}' /proc/$h/status)"
echo "   host log (warnings):"; grep -E "^\[[WE]\]" $RUN/F.log | sed 's/^/   | /' | head -6

echo; echo "== F2b) audio lost while the graph activates (one recorder: FL = filter input monitor, FR = sink)"
# lost_ms <tag> : play the 1.5 s tone; onset at the sink minus onset at the filter input, in ms
lost_ms() {
  local r
  r=$(rec_start_pair $RUN/F-act-$1.wav omadsp_spike_eq omadsp_spike_out); sleep 0.3
  play $RUN/F-sine0.wav omadsp_spike_out; sleep 0.3; rec_stop $r
  py - $RUN/F-act-$1.wav <<'PYEOF'
import sys, numpy as np, soundfile as sf
x, sr = sf.read(sys.argv[1], always_2d=True)
on = [np.nonzero(np.abs(x[:, c]) > 1e-3)[0] for c in (0, 1)]
if len(on[0]) == 0 or len(on[1]) == 0: print("no signal"); sys.exit()
print(f"{(on[1][0] - on[0][0]) / sr * 1000:.1f} ms")
PYEOF
}
signals sine $RUN/F-sine0.wav --freq 1000 --db -12 --secs 1 --lead 0 >/dev/null   # no lead-in silence to hide behind
acts() { grep -c "using n_samples" $1; }   # sofa instantiations so far (info log)
node_state() { pw-dump 2>/dev/null | python3 -I -c 'import json,sys; print(next((o["info"]["state"] for o in json.load(sys.stdin) if o.get("type")=="PipeWire:Interface:Node" and o["info"]["props"].get("node.name")==sys.argv[1]), "?"))' "$1"; }
echo "   sofa, first audio after host start (cold): $(lost_ms sofa-cold)"
echo "   host RSS after activation: $(awk '/VmRSS/ {printf "%.1f MiB", $2/1024}' /proc/$h/status)"
echo "   sofa, again right away (warm):            $(lost_ms sofa-warm)"
echo "   spatializer instances created so far: $(acts $RUN/F.log)"
cid=$(node_id omadsp_spike_eq)
set_params $cid '"spL:Azimuth" 60'; sleep 0.2   # runtime value; does it survive the suspend?
sleep 6; echo "   after 6 s idle: sink $(node_state omadsp_spike_out), filter output $(node_state omadsp_spike_eq_out); graph deactivations logged: $(grep -c "filter deactivate" $RUN/F.log)"
pactl suspend-sink omadsp_spike_out 1; sleep 0.5; pactl suspend-sink omadsp_spike_out 0; sleep 0.3
echo "   sofa, first audio after the suspend:       $(lost_ms sofa-resume)   (instances created so far: $(acts $RUN/F.log))"
echo "   IRs built on that resume (az el r): $(grep "making spatializer" $RUN/F.log | tail -2 | sed 's/.*with //' | tr '\n' ';')"
echo "   Props readback spL:Azimuth = $(pw-cli e $cid Props 2>/dev/null | grep -A1 '"spL:Azimuth"' | tail -1 | tr -s ' ')"
set_params $cid '"spL:Azimuth" 30'
echo "   host RSS now: $(awk '/VmRSS/ {printf "%.1f MiB", $2/1024}' /proc/$h/status)"

echo; echo "== F3) cues: what one channel turns into at the two ears"
signals imp $RUN/F-impL.wav --secs 1 --db 0 --only L >/dev/null
signals imp $RUN/F-impR.wav --secs 1 --db 0 --only R >/dev/null
signals pink $RUN/F-pinkL.wav --secs 3 --db -20 --only L >/dev/null
signals pink $RUN/F-pinkR.wav --secs 3 --db -20 --only R >/dev/null
for sig in impL impR pinkL pinkR; do
  r=$(rec_start_raw $RUN/F-$sig-rec.wav omadsp_spike_out); sleep 0.3
  play $RUN/F-$sig.wav omadsp_spike_out; sleep 0.3; rec_stop $r
done
py - $RUN <<'PYEOF'
import sys
import numpy as np, soundfile as sf
from scipy import signal
run = sys.argv[1]
def ld(n): return sf.read(f"{run}/F-{n}.wav", dtype="float64", always_2d=True)[0]
for side in "LR":
    ref, rec = ld(f"imp{side}"), ld(f"imp{side}-rec")
    k0 = int(np.argmax(np.abs(ref).sum(axis=1)))
    # rec starts at an unknown offset: align on the first sample of either ear above -40 dB of the peak
    pk = np.abs(rec).max()
    on = [int(np.nonzero(np.abs(rec[:, c]) > pk * 0.01)[0][0]) for c in (0, 1)]
    pos = [int(np.argmax(np.abs(rec[:, c]))) for c in (0, 1)]
    e = [10 * np.log10(np.sum(rec[:, c] ** 2) + 1e-30) for c in (0, 1)]
    near, far = (0, 1) if side == "L" else (1, 0)
    print(f"   impulse on {side}: ear onsets L/R differ by {on[far] - on[near]:+d} smp, peaks by {pos[far] - pos[near]:+d} smp; "
          f"energy near ear {e[near]:+.1f} dB, far ear {e[far]:+.1f} dB (ILD {e[near] - e[far]:.1f} dB)")
    x = ld(f"pink{side}-rec")
    a, b = x[:, near], x[:, far]
    lag = signal.correlation_lags(len(b), len(a))
    xc = signal.correlate(b, a, method="fft")
    w = np.abs(lag) <= 48
    best = lag[w][np.argmax(xc[w])]
    rms = lambda v: 10 * np.log10(np.mean(v ** 2) + 1e-30)
    print(f"   pink on {side}: far ear lags near ear by {best:+d} smp ({best / 48:.2f} ms); broadband ILD {rms(a) - rms(b):.1f} dB")
PYEOF

echo; echo "== F4) CPU while playing stereo pink noise (pw-top, 3 samples)"
signals pink $RUN/F-pink.wav --secs 6 --db -20 >/dev/null
p=$(play_bg $RUN/F-pink.wav omadsp_spike_out); sleep 2
pw-top -b -n 3 2>/dev/null | grep -E "omadsp_spike_eq|omadsp_spike_out|S +ID" | tail -6 | sed 's/^/   | /'
ticks() { awk '{print $14 + $15}' /proc/$1/stat; }
c0=$(ticks $h); sleep 3; c1=$(ticks $h)
echo "   host CPU over 3 s of playback: $(( (c1 - c0) * 100 / 300 )).$(( (c1 - c0) * 1000 / 300 % 10 ))% of one core (clock ticks $((c1 - c0)))"
kill $p 2>/dev/null; wait $p 2>/dev/null

echo; echo "== F5) live Azimuth: left speaker 30 <-> 90 degrees every 0.5 s while a 1 kHz tone plays on L"
signals sine $RUN/F-sineL.wav --freq 1000 --db -12 --secs 6 --only L >/dev/null
cid=$(node_id omadsp_spike_eq)
r=$(rec_start_raw $RUN/F-az.wav omadsp_spike_out); sleep 0.3
p=$(play_bg $RUN/F-sineL.wav omadsp_spike_out); sleep 1.3
for k in 1 2 3 4 5 6 7 8; do
  az=$(( k % 2 ? 90 : 30 )); t0=$(date +%s%N); set_params $cid "\"spL:Azimuth\" $az"
  printf "   set %2d -> pw-cli returned in %d ms\n" $az $(( ($(date +%s%N) - t0) / 1000000 ))
  sleep 0.5
done
while kill -0 $p 2>/dev/null; do sleep 0.05; done; sleep 0.3; rec_stop $r
echo "   Props readback spL:Azimuth = $(pw-cli e $cid Props 2>/dev/null | grep -A1 '"spL:Azimuth"' | tail -1 | tr -s ' ')"
measure gaps $RUN/F-az.wav --min-ms 0.5
for ch in L R; do
  py - $RUN/F-az.wav $ch $RUN/F-az-$ch.wav <<'PYEOF'
import sys, soundfile as sf
x, sr = sf.read(sys.argv[1], always_2d=True); c = x[:, 0 if sys.argv[2] == "L" else 1]
sf.write(sys.argv[3], __import__("numpy").stack([c, c], axis=1), sr, subtype="FLOAT")
PYEOF
  echo "   ear $ch: $(measure clicks $RUN/F-az-$ch.wav --freq 1000 --top 4)"
done
grep -c "delay dropped" $RUN/F.log | sed 's/^/   "delay dropped" warnings in host log: /'

stop_host $h; sleep 0.5

echo; echo "== F6) output level: SADIE D2 as-is vs config normalize = true (stereo pink -20 dBFS RMS in)"
sed 's|config = { filename = "@SOFA@" }|config = { filename = "@SOFA@" normalize = true }|' "$HERE/sofa.conf.in" >$RUN/F-sofa-norm.conf.in
signals pink $RUN/F-pink3.wav --secs 3 --db -20 >/dev/null
for v in as-is normalize; do
  [[ $v == as-is ]] && mkhost -D SOFA="$SOFA" $RUN/F6.conf "$HERE/sofa.conf.in" || mkhost -D SOFA="$SOFA" $RUN/F6.conf $RUN/F-sofa-norm.conf.in
  h=$(PIPEWIRE_DEBUG=3 start_host $RUN/F6.conf $RUN/F6-$v.log); wait_node omadsp_spike_eq_out >/dev/null; sleep 0.5
  r=$(rec_start_raw $RUN/F6-$v.wav omadsp_spike_out); sleep 0.3
  play $RUN/F-pink3.wav omadsp_spike_out; sleep 0.3; rec_stop $r
  echo "   $v: out RMS $(measure rms $RUN/F6-$v.wav)  peak $(measure peak $RUN/F6-$v.wav)"
  grep -o "blocksize [a-z]*gain:[0-9.]*" $RUN/F6-$v.log | sort | uniq -c | sed 's/^/      log: /'
  stop_host $h; sleep 0.5
done

echo; echo "== F2c) the same cold start with the LSP EQ + limiter chain (Spike E config), for comparison"
mkhost -D TH=0.891 $RUN/F-lsp.conf "$SPIKES/E-meters/meter.conf.in"
h=$(start_host $RUN/F-lsp.conf $RUN/F-lsp.log); wait_node omadsp_spike_eq_out >/dev/null; sleep 0.5
echo "   LSP, cold:  $(lost_ms lsp-cold)   (the limiter's 5 ms lookahead is in every LSP number)"
echo "   LSP, warm:  $(lost_ms lsp-warm)"
pactl suspend-sink omadsp_spike_out 1; sleep 0.5; pactl suspend-sink omadsp_spike_out 0; sleep 0.3
echo "   LSP, after a suspend: $(lost_ms lsp-resume)"
stop_host $h; sink_stop $s; sleep 0.4
check_clean
