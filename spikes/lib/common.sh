# shellcheck shell=bash
# Shared helpers for omadsp spikes. Source this file; nothing here writes outside
# the repo or $XDG_RUNTIME_DIR/omadsp-spike.

LIB="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SPIKES="$(dirname "$LIB")"
PY="$SPIKES/.venv/bin/python"
RUN="${XDG_RUNTIME_DIR:?}/omadsp-spike"
mkdir -p "$RUN"

py() { "$PY" -I "$@"; }
measure() { py "$LIB/measure.py" "$@"; }
signals() { py "$LIB/signals.py" "$@"; }
mkhost() { py "$LIB/mkhost.py" "$@"; }

# node_id <node.name>  -> PipeWire global id (empty if absent)
node_id() {
  pw-dump 2>/dev/null | python3 -I -c '
import json, sys
name = sys.argv[1]
for o in json.load(sys.stdin):
    if o.get("type") == "PipeWire:Interface:Node" and o["info"]["props"].get("node.name") == name:
        print(o["id"]); break
' "$1"
}

wait_node() {
  local id i
  for i in $(seq 1 60); do
    id="$(node_id "$1")"
    [[ -n $id ]] && { echo "$id"; return 0; }
    sleep 0.1
  done
  return 1
}

# sink_start [node.name] -> prints a process-group id. Creates a private null sink
# on the server, owned by a held-open pw-cli session; sink_stop kills the session,
# which destroys the sink. priority 0 so WirePlumber never picks it as default.
sink_start() {
  local name="${1:-omadsp_spike_out}"
  setsid bash -c '
    { printf "%s\n" "create-node adapter { factory.name = support.null-audio-sink node.name = $0 node.description = \"omadsp spike $0\" media.class = Audio/Sink audio.position = [ FL FR ] priority.session = 0 priority.driver = 0 monitor.channel-volumes = true state.restore-props = false state.restore-target = false }"
      exec sleep infinity; } | exec pw-cli' "$name" >/dev/null 2>&1 &
  echo $!
}

sink_stop() {
  [[ -n ${1:-} ]] || return 0
  kill -- "-$1" 2>/dev/null
  wait "$1" 2>/dev/null
  return 0
}

# start_host <conf> <log>  -> prints pid
start_host() {
  pipewire -c "$1" >"$2" 2>&1 &
  echo $!
}

stop_host() {
  [[ -n ${1:-} ]] || return 0
  kill "$1" 2>/dev/null
  wait "$1" 2>/dev/null
  return 0
}

# Stream props for every test stream: keep WirePlumber's state-stream from saving
# (and later restoring) volumes or targets for them in ~/.local/state/wireplumber.
NOSTATE="state.restore-props = false state.restore-target = false"
# Test playback streams also never fall back to (or get moved onto) the default
# device if their target disappears mid-test — the speakers stay out of it.
NOFALLBACK="node.dont-fallback = true node.dont-reconnect = true"

# play <wav> <target-node-name> [extra pw-cat args]
play() {
  local wav="$1" target="$2"
  shift 2
  pw-cat --playback --target "$target" -P "{ $NOSTATE $NOFALLBACK }" "$@" "$wav"
}

# rec_start <out.wav> <target-node-name>  -> pid; records the target's monitor (sinks) in f32
rec_start() {
  pw-record --target "$2" -P "{ stream.capture.sink = true node.name = omadsp_spike_rec $NOSTATE }" \
    --format f32 --rate 48000 --channels 2 "$1" >/dev/null 2>&1 &
  echo $!
}

# rec_start_raw <out.wav> <node.name> : record <node>'s monitor ports, linked by
# hand (node.autoconnect = false) so WirePlumber's smart-filter policy cannot swap
# the target for a filter in front of it. Waits until both links exist.
rec_start_raw() {
  local tag="omadsp_spike_rec_$RANDOM" pid i
  pw-record -P "{ node.autoconnect = false node.name = $tag $NOSTATE }" \
    --format f32 --rate 48000 --channels 2 "$1" >/dev/null 2>&1 &
  pid=$!
  for i in $(seq 1 50); do
    pw-link "$2:monitor_FL" "$tag:input_FL" 2>/dev/null && pw-link "$2:monitor_FR" "$tag:input_FR" 2>/dev/null && break
    sleep 0.05
  done
  echo "$pid"
}

# rec_start_pair <out.wav> <node-a> <node-b> : one recorder, FL = node-a monitor_FL,
# FR = node-b monitor_FL. Same clock and start for both, for latency measurements.
rec_start_pair() {
  local tag="omadsp_spike_rec_$RANDOM" pid i
  pw-record -P "{ node.autoconnect = false node.name = $tag $NOSTATE }" \
    --format f32 --rate 48000 --channels 2 "$1" >/dev/null 2>&1 &
  pid=$!
  for i in $(seq 1 50); do
    pw-link "$2:monitor_FL" "$tag:input_FL" 2>/dev/null && pw-link "$3:monitor_FL" "$tag:input_FR" 2>/dev/null && break
    sleep 0.05
  done
  echo "$pid"
}

# links_of <node.name> : what a node's input ports are connected from
links_of() {
  pw-link -l -I 2>/dev/null | awk -v n="$1" '$0 ~ "^ *[0-9]+ "n":" {p=1; print; next} /^ *[0-9]+ [^ |]/ {p=0} p'
}

rec_stop() {
  kill -INT "$1" 2>/dev/null
  wait "$1" 2>/dev/null
  return 0
}

# play_bg <wav> <target> -> pid of pw-cat itself (not a subshell), so kill stops playback
play_bg() {
  pw-cat --playback --target "$2" -P "{ $NOSTATE $NOFALLBACK }" "$1" >/dev/null 2>&1 &
  echo $!
}

# play_rec <wav> <play-target> <rec-target> <out.wav> : record while playing
play_rec() {
  local r
  r="$(rec_start "$4" "$3")"
  sleep 0.4
  play "$1" "$2"
  sleep 0.3
  rec_stop "$r"
}

# set_props <node-id> '<spa-json params array contents>'
set_params() {
  pw-cli s "$1" Props "{ params = [ $2 ] }" >/dev/null
}

nodes_snapshot() {
  pw-dump 2>/dev/null | python3 -I -c '
import json, sys
rows = set()
for o in json.load(sys.stdin):
    if o.get("type") != "PipeWire:Interface:Node": continue
    p = o["info"]["props"]
    if p.get("node.name", "").startswith(("omadsp_spike_rec",)): continue
    if p.get("media.class", "").startswith("Stream/"): continue
    rows.add(p.get("media.class", "") + "\t" + p.get("node.name", ""))
print("\n".join(sorted(rows)))
'
}

baseline() {
  nodes_snapshot >"$RUN/baseline.nodes"
  pw-metadata -n filters 0 2>/dev/null | grep -v '^Found' >"$RUN/baseline.filters"
  pw-metadata -n default 0 2>/dev/null | grep -v '^Found' >"$RUN/baseline.default"
  pw-metadata -n settings 0 2>/dev/null | grep -v '^Found' >"$RUN/baseline.settings"
}

check_clean() {
  local ok=0
  diff <(nodes_snapshot) "$RUN/baseline.nodes" >/dev/null || { echo "NODES DIFFER:"; diff <(nodes_snapshot) "$RUN/baseline.nodes"; ok=1; }
  diff <(pw-metadata -n filters 0 2>/dev/null | grep -v '^Found') "$RUN/baseline.filters" >/dev/null || { echo "FILTERS METADATA DIFFERS"; ok=1; }
  diff <(pw-metadata -n default 0 2>/dev/null | grep -v '^Found') "$RUN/baseline.default" >/dev/null || { echo "DEFAULT METADATA DIFFERS"; ok=1; }
  diff <(pw-metadata -n settings 0 2>/dev/null | grep -v '^Found') "$RUN/baseline.settings" >/dev/null || { echo "SETTINGS METADATA DIFFERS"; ok=1; }
  ((ok == 0)) && echo "clean: matches baseline"
  return $ok
}
