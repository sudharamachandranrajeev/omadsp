# Spike A′ — in-node filter graph (`audioconvert.filter-graph.N`) (results)

Run 2026-10-12, PipeWire 1.6.8. Graphs were set with
`pw-cli s <node> Props '{ params = [ "audioconvert.filter-graph.<N>" "<graph SPA-JSON>" ] }'`
on private nodes only — never the speakers. `run.sh` reproduces 1–6.

A sink's monitor turned out to be pre-graph (test 1), so the graph's effect can't be seen on the node that hosts
it. Tests 2–6 therefore put the graph on a `pw-loopback` capture node: an adapter `Audio/Sink` with the same
audioconvert as a device sink, whose output feeds the private sink `omadsp_spike_m1`, where it is recorded. The
loopback alone is bit-exact (null −∞, twice).

## Results

| # | Question | Result |
|---|---|---|
| 1 | Is a sink's monitor pre- or post-graph? | **Pre-graph.** Graph `linear Mult=0` on the sink: monitor still −20.0 dBFS. Source: the monitor copies `src_datas` (the node's input). Same trap as smart filters: monitor recorders get unprocessed audio. |
| 2 | Graph before or after node volume? | **Before.** `clamp ±0.5` graph, −3 dBFS tone, channel volume 0.5 → peak −12.04 dBFS (0.25), not −9 (0.354). Source: stage order convert → resample → **graphs** → channelmix (volume) → convert. |
| 3 | Live control key format | Plain **`"node:port"`** (`"g:Mult" 1.0`: −26 → −20 dBFS). `"0:g:Mult"` is ignored. Props go to *every* graph on the node, so node names must be unique across all graphs. |
| 4 | LV2 inside a node | Works. LSP `limiter_stereo`, th 0.25: −3 dBFS → −13.1 dBFS; live `"lim:th" 1.0` → −3.0 dBFS. |
| 5 | Replacing a graph (same N) while playing | **No gap** (0 silent runs ≥ 0.5 ms over 10 swaps), but no crossfade. Swapping between identical graphs is seamless (no click windows). The swap from the LSP limiter (240 smp lookahead, −20 dBFS) to `linear 0.5` produced a **−9 dBc click**. Any change of latency, gain or filter state is a hard cut. |
| 6 | `audioconvert.filter-graph.disable` | **Not a bypass, and a one-way latch.** With a graph loaded, `disable = true` leaves it processing (−26 dBFS) and refuses new graphs. `disable = false` is ignored afterwards (readback stays `true`), and the loaded graph **can no longer be removed** (`""` is refused too) until the node is destroyed. Source: `parse_prop_params` starts from the current flag and only ever sets it (`if (!*disable_filter) *disable_filter = ...`); `load_filter_graph` returns `-EPERM` before handling removal. PipeWire bug. |
| 7 | Graph size | Each Props param value is copied into `char value[4096]`. A 4200-byte graph is **silently dropped** (no error to the setter); up to ~4095 bytes loads. A fully spelled-out 32-band stereo EQ doesn't fit. |
| 8 | Who owns device nodes / survival | `alsa_output…analog-stereo` is created by **WirePlumber's** client (`client.id` → wireplumber, no `object.linger`). A WirePlumber restart, card profile change or BT reconnect destroys and recreates the node, and the graph with it. The daemon would have to re-apply on every node appearance, alongside WirePlumber's own `node.filter-graph.rules` indices. Not restarted live (it would interrupt the voice-control recorder). |
| 9 | Setter gone | The graph lives on the node; `pw-cli` exits after each set and the graph keeps running (all tests above). |
| 10 | Where the DSP code runs | **Inside the PipeWire daemon.** An LV2 graph on a server-side node (the null sink, created like ALSA nodes) mapped `liblilv`, `serd`, `sord`, `sratom`, `lsp-plugins-lv2.so` and LSP's `/dev/shm/lsp-catalog-*` into `/usr/bin/pipewire` (pid 939). A plugin crash would take down PipeWire and every client. |

## Verdict

Mechanically workable — and it processes pre-volume, which is right for a limiter — but **not the primary
path**:
- third-party LV2 code inside the PipeWire daemon (10) versus a separate host process for smart filters;
- graphs vanish with WirePlumber-owned device nodes (8);
- a 4 KiB graph cap (7);
- a latch bug that can strand a graph on a device until the node is recreated (6);
- hard cuts on every replace (5);
- the monitor is pre-DSP anyway (1), so there's no metering advantage.

Smart filters (Spike A) stay primary. Not done: the 👂 in-node click check on the speakers. The graph's effect
can't be measured there (monitor pre-graph, no loopback capture on the ALC3204); the click behaviour belongs to
the DSP, not the host (Spike A §3); and a stranded graph would need a WirePlumber restart to clear.
