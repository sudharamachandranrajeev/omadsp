#!/usr/bin/env bash
# Spike C — hosting: child `pipewire -c` process vs pw_context_load_module() in our own process.
# Private sink only. The filter is the LSP chain (flat) targeting omadsp_spike_out; the test stream
# targets omadsp_spike_out, so while no filter exists WirePlumber links it straight to the sink.
set -u
source "$(dirname "$0")/../lib/common.sh"

cc -O2 -Wall -o $RUN/inproc "$SPIKES/C-hosting/inproc.c" $(pkg-config --cflags --libs libpipewire-0.3) || exit 1
mkhost $RUN/C.conf "$SPIKES/A-smart-filter/lsp-peq.conf.in"
# the filter-chain `args = { ... }` object, for pw_context_load_module
py - "$SPIKES/A-smart-filter/lsp-peq.conf.in" $RUN/C-args.conf <<'EOF'
import sys
t = open(sys.argv[1]).read()
i = t.index("{", t.index("args ="))
d = 0
for j in range(i, len(t)):
    d += {"{": 1, "}": -1}.get(t[j], 0)
    if d == 0:
        break
open(sys.argv[2], "w").write(t[i:j + 1])
EOF
signals sine $RUN/sine1k-8.wav --freq 1000 --db -20 --secs 8 >/dev/null
PORT=omadsp_spike_eq_out:output_FL
ns() { date +%s%N; }
# wait_port -> ms until the filter's output port exists (polled every ~5 ms)
wait_port() { local t0=$1; until pw-link -o 2>/dev/null | grep -q "^$PORT\$"; do sleep 0.005; done; echo $(( ($(ns) - t0) / 1000000 )); }
wait_gone() { while pw-link -o 2>/dev/null | grep -q "^$PORT\$"; do sleep 0.005; done; }
threads() { ps -L -o tid=,cls=,rtprio=,comm= -p "$1" | awk '{printf "      %-8s %-3s %-3s %s\n", $1, $2, $3, $4}'; }
rss() { awk '/VmRSS/ {print $2/1024 " MiB"}' /proc/$1/status; }

s=$(sink_start); wait_node omadsp_spike_out >/dev/null

if [[ -z ${SKIP_C1:-} ]]; then
echo "== C1) time to a usable filter (output port visible), 5 runs each"
c=""; i=""
for k in 1 2 3 4 5; do
  t0=$(ns); h=$(start_host $RUN/C.conf $RUN/C.log); c+=" $(wait_port $t0)"; stop_host $h; wait_gone
  t0=$(ns); $RUN/inproc $RUN/C-args.conf 0 30 >$RUN/C-inproc.log 2>&1 & ip=$!; i+=" $(wait_port $t0)"; kill $ip; wait $ip 2>/dev/null; wait_gone
done
echo "   child pipewire -c (spawn -> port), ms:$c"
echo "   in-process (exec + load_module -> port), ms:$i"
fi

echo; echo "== C2) reload while playing a 1 kHz tone: 3 reloads, 1.5 s apart"
echo "   -- in-process: pw_impl_module_destroy + pw_context_load_module"
r=$(rec_start_raw $RUN/C-rl-in.wav omadsp_spike_out); sleep 0.3
$RUN/inproc $RUN/C-args.conf 3 1.5 >$RUN/C-inproc.log 2>&1 & ip=$!
wait_port $(ns) >/dev/null; sleep 0.5
p=$(play_bg_relink $RUN/sine1k-8.wav omadsp_spike_out); sleep 0.5
echo "   threads of the in-process host (tid class rtprio comm):"; threads $ip
echo "   RSS: $(rss $ip)"
wait $ip 2>/dev/null
while kill -0 $p 2>/dev/null; do sleep 0.1; done; sleep 0.3; rec_stop $r
grep -E "^(load|destroy)" $RUN/C-inproc.log | awk '{printf "      %s took %s ms\n", $1, $4}'
measure gaps $RUN/C-rl-in.wav --min-ms 0.5; measure clicks $RUN/C-rl-in.wav --freq 1000 --top 8
echo "   -- child: kill + respawn"
h=$(start_host $RUN/C.conf $RUN/C.log); wait_port $(ns) >/dev/null; sleep 0.5
r=$(rec_start_raw $RUN/C-rl-ch.wav omadsp_spike_out); sleep 0.3
p=$(play_bg_relink $RUN/sine1k-8.wav omadsp_spike_out); sleep 0.5
echo "   threads of the child host:"; threads $h
echo "   RSS: $(rss $h)"
for k in 1 2 3; do
  sleep 1.5; t0=$(ns); stop_host $h; h=$(start_host $RUN/C.conf $RUN/C.log); echo "      respawn $k: port after $(wait_port $t0) ms"
done
while kill -0 $p 2>/dev/null; do sleep 0.1; done; sleep 0.3; rec_stop $r
measure gaps $RUN/C-rl-ch.wav --min-ms 0.5; measure clicks $RUN/C-rl-ch.wav --freq 1000 --top 8

echo; echo "== C3) child host dies (SIGKILL — same effect on audio as a crash, without a core dump or a crash"
echo "   notification) while playing: what happens to the stream?"
r=$(rec_start_raw $RUN/C-crash.wav omadsp_spike_out); sleep 0.3
p=$(play_bg_relink $RUN/sine1k-8.wav omadsp_spike_out); sleep 1.5
kill -KILL $h; sleep 0.05; kill -0 $h 2>/dev/null && echo "   host still alive?" || echo "   host gone"
sleep 1; echo "   stream now linked to:"; pw-link -l 2>/dev/null | grep -A1 "^pw-cat:output_FL" | tail -1
h=$(start_host $RUN/C.conf $RUN/C.log); wait_port $(ns) >/dev/null; sleep 1
echo "   after respawn, stream linked to:"; pw-link -l 2>/dev/null | grep -A1 "^pw-cat:output_FL" | tail -1
while kill -0 $p 2>/dev/null; do sleep 0.1; done; sleep 0.3; rec_stop $r
measure gaps $RUN/C-crash.wav --min-ms 0.5; measure clicks $RUN/C-crash.wav --freq 1000 --top 4
stop_host $h

sink_stop $s; sleep 0.4
check_clean
