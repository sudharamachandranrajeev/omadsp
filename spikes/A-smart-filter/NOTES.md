# Spike A — smart filter (results)

Run 2026-10-12 on PipeWire 1.6.8, WirePlumber 0.5.17, ALC3204 at 48 kHz. Unless marked 🔊, every test ran against
the private null sink `omadsp_spike_out` (see `../lib/common.sh`), not the speakers. Recorders were linked by hand to
`<sink>:monitor_*` (`rec_start_raw`) — see finding 1 for why that matters.

Files: `filter.conf.in` (10× builtin `bq_peaking` per channel + LSP `limiter_stereo`), `lsp-peq.conf.in`
(LSP `para_equalizer_x16_lr` + limiter), `ramp-gain.conf.in` (builtin `ramp` × `mult`, broken — see 6).

## 1. Smart filters do process audio: Omarchy's "passes unprocessed" was a measurement trap

+6 dB at 1 kHz (`eqL_5:Gain`/`eqR_5:Gain`), −12 dBFS tone in:

| Recorder | 1 kHz level |
|---|---|
| hand-linked to `omadsp_spike_out:monitor_*` | **−6.0 dBFS** (processed) |
| `pw-record --target omadsp_spike_out` + `stream.capture.sink = true` (autoconnect) | −12.0 dBFS |

The autoconnected recorder is not where it was asked to be. WirePlumber's `get-filter-from-target.lua` treats
*any* stream aimed at the smart filter's target — including a capture stream on its monitor — as a stream to
divert, and links it to `omadsp_spike_eq:monitor_*`, i.e. the filter's **input**. Omarchy's comment in
`tunings/dell-xps-2026/filter-chain.conf` ("the filter's input monitor and the speaker sink's monitor measure
identically") describes exactly this.

Product consequences:
- Anything that records "the output device's monitor" through WirePlumber (OBS desktop audio, `parec` on the
  monitor, visualisers) gets **pre-DSP** audio while a smart filter is active. Document it; meters must link by hand.
- All omadsp test tooling must hand-link recorders.

## 2. Accuracy and transparency

| Test | Result |
|---|---|
| Flat EQ (10 bands at 0 dB) + limiter (th 1.0, alr off) | **bit-identical** (pink noise, after active-span trim) |
| V-curve, and one band +15 dB / Q 4, vs RBJ cookbook (`../lib/rbj.py`) | max error **0.026 dB** over 96 tones |
| LSP para_eq bell 1 kHz + low shelf 105 Hz + high shelf 8 kHz (APO mode) vs RBJ | max error **0.015 dB** |
| LSP para_eq, bands `Off` | bit-identical |
| LSP para_eq, `enabled = 0` | bit-identical |
| LSP para_eq, bands on at gain 1.0 (0 dB) | null −95 dB peak — not bit-exact, so flat bands must be switched `Off` |

## 3. Live parameter changes — builtin biquads click, LSP doesn't

1 kHz tone at −20 dBFS, parameter driven 20×/s, residual after notch + 4 kHz high-pass, 5 ms windows:

| What changes | Residual peak | Windows over −60 dBc |
|---|---|---|
| builtin `bq_peaking` Gain (388 updates) | **−16 dB** | 432 |
| LSP para_eq `gl_0` (same sweep) | −62 dB | 1 |
| LSP para_eq `enabled` toggled every 0.5 s | −63.7 dB | 31 |
| LSP para_eq `g_out` sweep 0.25–1.0 | −55.5 dB | 339 (zipper) |

Root cause (PipeWire 1.6.8 source): `biquad_set()` in `spa/plugins/audioconvert/biquad.c` begins with
`/* Clear history values. */ bq->x1 = 0; bq->x2 = 0;`, and `bq_run` calls it whenever Freq/Q/Gain changed.
`bq_raw` zeroes the same state on coefficient changes. Every update resets the filter → a click. **Builtin biquads
are unusable for anything the user can drag.** (An upstream patch to keep state is a possible long-term fix.)

## 4. De-zippered gain: audioconvert's volume ramp works, if you also give it a step

Setting `Props { volume: g, volumeRampTime: <ms>, volumeRampStepSamples: <n> }` on the filter's **playback**
node (`omadsp_spike_eq_out`) makes audioconvert generate a sample-accurate ramp. Without
`volumeRampStepSamples`/`volumeRampStepTime` the ramp is silently dropped (`generate_ramp_seq` returns NULL when
`step == 0`).

Gain swept −12…0 dB, a new target every 50 ms (or 20 ms, overlapping ramps):

| Mode | Residual peak | Windows over −60 dBc |
|---|---|---|
| step (no ramp), 50 ms | −49.5 dB | 290 |
| ramp 30 ms, step 8 samples, 50 ms | −67.4 dB | 3 |
| ramp 30 ms, step 8 samples, 20 ms (overlap) | −71.4 dB | 1 |
| ramp 15 ms, step 4 samples, 20 ms | −71.6 dB | 1 |
| ramp 30 ms, step 48 samples | −70.0 dB | 525 (1 ms staircase) |

→ Preamp, DVC and bypass fades go on the playback node's `volume` with a ramp and step ≤ 8 samples. No one else
touches that node's `volume` (Omarchy and WirePlumber act on the sink side).

## 5. WirePlumber persists volumes of filter nodes — opt out

`state-stream.lua` saves **and restores** `volume`/`mute`/`channelVolumes` for every `Audio/*` node with no
`device.routes`, which includes filter-chain nodes, virtual sinks and test streams. Before this was noticed the
spikes left 7 entries in `~/.local/state/wireplumber/stream-properties` (see the Phase 0 report). With
`state.restore-props = false` and `state.restore-target = false` on capture, playback, the test sink and the test
streams, the file stayed byte-identical across all later runs.

→ Every omadsp node sets both. Otherwise WirePlumber would rewrite its state file on every slider drag and
restore stale gains at next start, fighting the daemon.

## 6. Builtin `ramp` is broken in 1.6.8

`ramp_ports` declare no hints, so def/min/max are all 0, and `port_set_control_value` clamps every value —
config or live — to `[0, 0]`. Start = Stop = Duration = 0 → the ramp outputs 0 and `ramp × signal` is silence
(confirmed with `PIPEWIRE_DEBUG=4`: `control 0 0 ('Start') from 0.000000 to 0.000000` although the config sets
Stop = 1.0). Don't use it. The two-chain crossfade idea that needed it is dropped; finding 4 replaces it.

## 7. Latency and CPU

One recorder, FL = `omadsp_spike_eq:monitor_FL` (filter in), FR = `omadsp_spike_out:monitor_FL` (filter out),
so both channels share a clock and start (`measure.py xlat`). Pink noise.

| Chain | Latency | Host CPU (one core) |
|---|---|---|
| builtin 10-band + LSP limiter, lookahead 5 ms, ovs off | **240 smp = 5.0 ms** | 0.55 % |
| same, ovs 22 (true peak, 24-bit) | 364 smp = 7.6 ms | 1.1 % |
| LSP para_eq x16 + limiter (lookahead 5 ms) | 240 smp = 5.0 ms | 0.27–0.66 % |
| LSP para_eq x16 + limiter at quantum 256 (`node.latency`) | — | 0.99 %, 0 ERR |
| builtin 10-band + limiter TP at quantum 256 | — | 1.33 %, 0 ERR |
| builtin 10-band, dragging 20×/s | — | 1.09 % |

The filter-chain itself adds **0 samples** (capture and playback run in the same cycle). All latency is the
limiter's lookahead; true-peak oversampling adds 124 samples. CPU is far under the 2 % budget.

## 8. 🔊 Speakers: targetless smart filter and default switch

Targetless LSP chain (+6 dB bell at 1 kHz) with the real default (`alsa_output…analog-stereo`, "Speaker Output"):

- WirePlumber linked `omadsp_spike_eq_out` → speakers on its own; `default.audio.sink` stayed the speakers.
- A plain `pw-cat` with no target landed on the filter; the speaker sink's monitor read **−24.0 dBFS** for a
  −30 dBFS tone (+6.0 dB; direct reference −30.0). The speaker sink showed ERR = 1 in `pw-top` (no baseline was
  taken before, so it may predate the test); the filter nodes showed 0.
- Switching `default.configured.audio.sink` to `bt_combined` moved the filter's output to `bt_combined` within
  1 s; switching back moved it to the speakers. The filter follows the default — no daemon logic needed.
- Restored in two steps (speakers, then the BT headset `bluez_output.D0_65_F3_86_70_0C.1`) because WirePlumber
  keeps configured defaults as a most-recent-first stack; afterwards `default-nodes` was byte-identical to
  before. WirePlumber did re-save `stream-properties` during the switch (same size; most likely the
  "Combine stream" line re-serialised with the same values).

## 9. Omarchy picker (code, not run)

`plugins/panels/audio/Panel.qml` lists every node with `isSink && !isStream`, minus sinks that
`omarchy-audio-sink-availability` reports unavailable. That script only hides sinks whose ports are all
unavailable, or the one sink that Omarchy's own speaker tuning fronts. A smart filter must be `Audio/Sink`
(WirePlumber's `filter-utils.lua` skips any main node whose class contains "Stream"), and it has no ports, so
**it will be listed as an output**. Picking it runs `wpctl set-default <filter>`. `find-best-default-node.lua`
skips smart filters, but `find-selected-default-node.lua` honours a configured default without that check, so
the filter would become the default sink.

Not tested live: it would leave a permanent `omadsp_spike_eq` entry in WirePlumber's configured-defaults stack.
Mitigations go in the Phase 0 report (clear naming; the daemon watches `default.configured.audio.sink` and
warns/repairs; an Omarchy upstream change to skip `filter.smart` nodes).
