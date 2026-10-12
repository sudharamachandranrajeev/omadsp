# Spike D — DVC (Direct Volume Control) (results)

Run 2026-10-12. `dvc.conf.in`: the filter sink's own volume mapped into the graph with
`capture.volumes = [ { control = "volL:Mult" } { control = "volR:Mult" } ]` → two builtin `linear` nodes in
front of the LSP limiter. Private sink only.

## Mechanics

| Test | Result |
|---|---|
| Filter sink `channelVolumes = [ 0.5 0.5 ]` | output −20.0 → **−26.02 dBFS**: applied in the graph |
| Double application? | No — `softVolumes` stays `[ 1.0 1.0 ]`; audioconvert hands the volume to the mapped controls |
| Dragging that volume 20×/s (0.25–1.0) | zipper: residual −52.6 dB, 132 windows over −60 dBc (the builtin `linear` steps) |

## Who drives which volume (code, not run)

`omarchy-audio-output-volume` (volume keys, OSD), the audio panel slider and the output switcher all act on
`omarchy-audio-output-sink`. That script's stated intent: *"Resolve through any DSP sink to the physical one, so
the keys always move real loudness and the processing always sees full-scale input."* With a smart filter the
default sink stays the physical one (Spike A §8), so nothing in Omarchy ever touches the filter sink's volume.
`capture.volumes` would only matter if the filter sink became the default (Spike A §9).

I didn't run `omarchy-audio-output-volume +5` live: the code is unambiguous, and running it would change your
speaker volume, show the OSD and rewrite WirePlumber's `default-routes`.

`omarchy-audio-output-sink` follows a DSP sink to the physical one through the sink input whose `node.name`
*starts with* the DSP sink's name. → **Naming rule: the playback node is `<capture node.name>` + a suffix**
(e.g. `omadsp` / `omadsp_out`). Then even if a user picks the omadsp sink in the panel, the volume keys still
land on the real device.

## Verdict: no Poweramp-style DVC in v1

In PipeWire the user's volume — hardware mixer, or PipeWire's soft volume on the device node — is applied
*after* omadsp's chain, so the EQ and limiter always run at full scale. Poweramp's DVC moves volume *before* the
DSP to gain headroom at low levels. Doing that here means pinning the device volume and intercepting the keys,
slider and OSD, which fights Omarchy's explicit design and its UI.

Instead:
- **Headroom (Poweramp's "No-DVC headroom gain"):** an automatic preamp of −(largest positive EQ boost), with a
  user offset, and the LSP limiter as the safety net. Set on preset change, not dragged, so steps are fine. Apply
  it inside a short dip of the ramped playback `volume` (Spike A §4) to hide them.
- **Optional "omadsp level" trim:** the ramped playback-node `volume` (post-limiter, click-free). It's a trim,
  not a headroom tool.
- `capture.volumes` is useful only for the "filter picked as default" edge case. Map it onto a smooth control
  in a later phase if that case matters.
- Bluetooth absolute volume: deferred (no working adapter during Phase 0). It's device volume like any other —
  after the chain — so this verdict doesn't change.
