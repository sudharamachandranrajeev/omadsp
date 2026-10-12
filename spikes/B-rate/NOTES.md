# Spike B — sample rates, runtime retargeting, bypass by relinking (results)

Run 2026-10-12, PipeWire 1.6.8 / WirePlumber 0.5.17. `run.sh` reproduces everything. Private sinks only:
`omadsp_spike_out` (48 kHz) and `omadsp_spike_m44`, which drives its own graph with `node.force-rate = 44100`.
`pw-top` during the 44.1 kHz run: `omadsp_spike_m44` R 1024 **44100**, `alsa_output…analog-stereo` R 1024
**48000**, so the hardware graph was untouched and no global `clock.force-rate` was needed (one would have
reconfigured the ALSA capture used by the voice-control recorder).

Re-scoped from the plan: the "bq_raw pool" question is settled in source — `bq_raw_update` zeroes the filter
state just like `biquad_set` (Spike A §3) — so B tests rates and retargeting with the LSP chain instead.

## B1 — LSP para_equalizer is accurate at both rates

Bell 1 kHz +6.02 dB Q 1.414, low shelf 105 Hz +4 dB, high shelf 8 kHz −3 dB (APO mode), 96-tone multitone at the
graph's own rate, compared with the RBJ cookbook at that rate:

| Graph rate | Max error vs RBJ |
|---|---|
| 48 000 | 0.007 dB |
| 44 100 | 0.008 dB |

The RBJ curves for the two rates differ by at most 0.038 dB (at 11.2 kHz) for these bands, so this mainly shows
LSP recomputes its coefficients for the actual rate and stays exact. It is not a strong discriminator between
"right rate" and "48 kHz coefficients".

## B2 — moving the filter between graphs at different rates costs ~20 ms

The filter was retargeted at runtime through the `filters` metadata
(`pw-metadata -n filters <filter-node-id> filter.smart.target '{ "node.name": "…" }' Spa:String:JSON`, deleted to
go back). WirePlumber reads this before the node's own `filter.smart.target`, so the daemon can retarget without
restarting the host. The test stream was linked to the filter itself, as apps are under a targetless filter. Six
moves 48 kHz ↔ 44.1 kHz while playing 8 s of pink noise:

- delivered 4.92 s on the 48 kHz sink + 2.96 s on the 44.1 kHz sink → **0.125 s lost over 6 moves (~21 ms
  each)**;
- host alive, no errors in its log. B1's 44.1 kHz measurement was taken after such a move, so the chain is
  correct on the far side of a rate change.

Fine for device changes. (Note: `clock.allowed-rates` is `[ 48000 ]` here, PipeWire's default, so all device
graphs normally run at 48 kHz and cross-rate moves only happen with a forced rate or a user-widened rate list.)

A first attempt retargeted the filter while the stream targeted `omadsp_spike_out`. That moves the *stream*, not
the filter: with no filter in front of its target, WirePlumber linked it straight to the sink. So **a filter
with a target only catches streams whose resolved target (explicit, or the default) is that device**. A
targetless filter catches everything that follows the default.

## B3 — `filter.smart.disabled` is not a usable bypass

Toggled every 0.5 s through the `filters` metadata while playing a 1 kHz tone (chain flat): WirePlumber relinks
every stream between the filter and the sink.

- first disable: a **42.7 ms** silent gap;
- every toggle: a click, **−12.4 dBc** (disable) and −26.2 dBc (enable). The paths differ by the limiter's 5 ms
  lookahead, so the waveform jumps.

→ Bypass stays inside the chain: ramp the playback node's `volume` down, flip LSP `enabled`, ramp up (Spike A
§3–4). `filter.smart.disabled` is only for "omadsp off" (host kept alive, nothing processed), where one click is
acceptable.
