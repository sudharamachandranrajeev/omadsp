# Spike E — meters: a source for the overload LED (results)

Run 2026-10-12, PipeWire 1.6.8, Quickshell 0.3.1. `run.sh` reproduces it. Chain (`meter.conf.in`): LSP
para_equalizer bell 1 kHz +6 dB → LSP limiter, threshold −1 dBFS, lookahead 5 ms. A −3 dBFS 1 kHz tone is
+3 dBFS after the EQ, so it gets limited. A −12 dBFS tone comes out at −6 dBFS and is not limited. Private sink
only.

## E1 — the limiter's meters never leave the graph

| Node | Controls in `Props` | Limiter meter outputs (`grlm_*`, `ilm_*`, `olm_*`, `sclm_*`) |
|---|---|---|
| `omadsp_spike_eq` (filter sink) | 378 | **none** |
| `omadsp_spike_eq_out` (filter output) | 0 | none |

Checked while the limiter was working. This matches the 1.6.8 source: `impl_get_props()` in
`spa/plugins/filter-graph/filter-graph.c` walks only `graph->control_port` (input controls). Output control
ports (`notify_port`) are used only inside the graph: for the latency report and for links to other nodes'
control inputs. No param event or `pw-dump` field carries gain reduction. So **no gain-reduction meter in v1**.

## E2 — a daemon-side tap on the filter output works

A capture stream with `node.autoconnect = false`, linked by hand to `omadsp_spike_eq_out:output_FL/FR`.
Because WirePlumber doesn't link it, smart-filter diversion can't move it. Peak per 50 ms window:

| Signal | Post-chain tap (peak; windows within 0.1 / 0.5 dB of the −1 dB ceiling) | Pre-chain (filter sink monitor) |
|---|---|---|
| tone −3 dBFS (limited) | −1.00 dBFS; **74/74** windows at the ceiling | −3.00 |
| tone −12 dBFS (not limited) | −5.98 dBFS; 0/74 | −12.00 |
| pink −12 dBFS RMS, ovs off (limited) | −1.00; 37 / 45 of 74 | +0.02 |
| pink −12 dBFS RMS, **true peak** (ovs 22) | −1.00; 20 / **44** of 74 | +0.02 |
| pink −30 dBFS RMS (not limited) | −17.34; 0 / 0 | −17.98 |

Nothing ever exceeded the ceiling. In true-peak mode the *sample* peaks sit up to ~0.5 dB below the ceiling
while limiting, so the test needs a 0.5 dB margin.

The pre-chain level alone can't drive the LED. The −3 dBFS tone looks safe at the input, and the EQ boost is
what pushes it over.

## E3 — Quickshell `PwNodePeakMonitor` only ever sees pre-DSP audio

`PwNodePeakMonitor` (source read: `src/services/pipewire/peak.cpp`, v0.3.1) is an autoconnected full-rate
capture stream (`stream.monitor = true`, `target.object = <serial>`). It reports `cbrt(peak)` and divides by
the node's soft volume. WirePlumber links it like any recorder. Where each of three monitors ended up:

| Monitor aimed at | Actually linked from | Reads (hot / safe tone) |
|---|---|---|
| `omadsp_spike_out` (the device stand-in) | `omadsp_spike_eq:monitor_FL/FR` | −3.00 / −12.00 |
| `omadsp_spike_eq` (filter sink) | `omadsp_spike_eq:monitor_FL/FR` | −3.00 / −12.00 |
| `omadsp_spike_eq_out` (filter output stream) | `omadsp_spike_eq:monitor_FL/FR` | −3.00 / −12.00 |

Quickshell flags all three as `stream.capture.sink = true`, and WirePlumber diverts every one of them to the
filter sink's monitor, which is pre-chain. Quickshell can't see post-DSP audio while omadsp is active. That
fits the guardrail anyway: the widget doesn't meter in QML.

Side effect: WirePlumber re-saved the existing `Input/Audio:application.name:Quickshell\sPeak\sDetect` entry
in `~/.local/state/wireplumber/stream-properties` with a stereo channel map (MONO → FL/FR; every volume still
1.0). Omarchy's own audio panel creates the same kind of stream, and nothing in Quickshell lets a stream opt
out.

## Verdict: overload LED

- **Source: the daemon's own post-chain tap.** A passive capture stream (`node.autoconnect = false`,
  `node.passive = true`), hand-linked to the playback node's output ports.
  - **LED on** when a 50 ms window's peak is within **0.5 dB of the limiter ceiling**, held ~0.5 s. Either the
    limiter is working, or the output is sitting at the ceiling.
  - The daemon pushes the state change to the widget over IPC, never per-quantum.
  - Run the tap only while something is subscribed (widget visible, `omadsp meter`), the way Omarchy's panel
    enables its monitor only while open.
- **Not available:** gain-reduction amount (E1), Quickshell meters (E3).
- **Untested alternative:** route `limiter:grlm_l` into a builtin's control input and out as an extra AUX
  channel. That would add channels to the playback node, and WirePlumber/channelmix handling of them is
  unknown. Not worth it for an LED.
