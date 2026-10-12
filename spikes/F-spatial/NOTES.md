# Spike F — spatial: HeSuVi example and the sofa spatializer (results)

Run 2026-10-12, PipeWire 1.6.8, libmysofa 1.3.5. `run.sh` reproduces it. HRTF: SADIE II **D2** (KEMAR dummy
head), 48 kHz, 256-tap FIR, from https://sofacoustics.org/data/database/sadie/ (University of York SADIE II
database, Apache-2.0 when downloaded; `assets/`, git-ignored). Chain (`sofa.conf.in`): two `sofa`/`spatializer`
nodes (L speaker at azimuth 30°, R at 330°, elevation 0, radius 1.2 m) summed per ear by two builtin
`mixer`s, as a smart filter on the private sink.

## Plugins are created when audio starts, not when the module loads

filter-chain instantiates the plugins in `activate_graph()` when its playback stream starts streaming. It
destroys them when the stream's format is cleared, which happens whenever the sink suspends: WirePlumber
suspends an idle sink after ~5 s, and `pactl suspend-sink` does it on demand. So **loading the module checks
nothing about the plugins' files**, and every resume after an idle period instantiates all plugins again.

## F1 — HeSuVi 7.1 example: a broken graph swallows all audio

`sink-virtual-surround-7.1-hesuvi.conf` as shipped (own node names, as a smart filter):
- The module **loads fine** and the filter nodes appear.
- On the first audio, the convolvers can't open `hrir_hesuvi/hrir.wav`. It is a *relative* path, and the
  builtin convolver `sf_open()`s it from the host process's working directory: "Can't open any sample file
  (CWD …)" → "cannot create plugin instance" → playback stream error "can't start graph".
- **The stream stays linked to the dead filter and the sink receives pure silence.** The host keeps running,
  and WirePlumber doesn't route around it.
- No `hrir_hesuvi`/`hrir.wav` is installed anywhere on the system (expected: HeSuVi IRs aren't packaged).

→ The daemon must check every file a graph references (IRs, SOFA) before loading it, always pass absolute paths,
and treat a filter stream in `error` state as a failure: tear the filter down (streams then go straight to the
device, Spike C) and report it.

## F2 — sofa load, activation, memory

| | |
|---|---|
| Spawn → filter port visible | 20–22 ms (SOFA file 35 MB; nothing opened yet) |
| Host RSS before / after first audio | 9.0 → **32.2 MiB** (HRTF set loaded by libmysofa) |
| Two spatializers instantiated, IRs built | ~1.4 ms (info log timestamps) |
| Audio lost at a cold start, a warm start, and after a suspend (onset at the sink minus onset at the filter input, one recorder) | **0.1 ms** each (the IR's own onset). LSP EQ + limiter: 5.0 ms each, its lookahead |
| Runtime control value across a suspend/resume | **kept**. `spL:Azimuth` set to 60 at runtime; after the resume the IR was rebuilt at 60° and Props read back 60 |

The daemon doesn't have to re-push controls after an idle suspend.

## F3 — the spatial cues are right

One channel in, both ears out (pink noise and impulses, both sides):

| Source | Far-ear delay (ITD) | Broadband ILD (pink) | Impulse energy, near vs far ear |
|---|---|---|---|
| L speaker (30°) | 12 smp = **0.25 ms** | 3.6 dB | +14.7 / +4.5 dB |
| R speaker (330°) | 13 smp = **0.27 ms** | 3.7 dB | +14.2 / +3.5 dB |

The expected ITD for 30° on a KEMAR-sized head is ~0.26 ms (Woodworth). The plugin discards SOFA `Data.Delay`
(`// TODO: make use of delay` in `plugin_sofa.c`), but D2's FIRs carry the delay inside the IRs, so nothing is
lost. No "delay dropped" warnings were logged. A SOFA set that stores delays separately *would* lose its ITD.
Check for those warnings when choosing an HRTF.

## F4 — CPU

`pw-top` at quantum 2048: BUSY ≈ 70 µs (filter output) + 11 µs (filter input) per 42.7 ms cycle → **~0.2 %**
of a core. Host CPU over 3 s from `/proc/<pid>/stat`: 0.3 % (one 10 ms tick, so coarse). Spike A's LSP EQ +
limiter was 0.27–0.66 %, so EQ + spatial + limiter fits the 2 % budget comfortably.

## F5 — live Azimuth works

`pw-cli s <id> Props '{ params = [ "spL:Azimuth" 90 ] }'` toggled 30↔90 every 0.5 s while a 1 kHz tone played
on L:
- `pw-cli` returns in 6–8 ms;
- **no gaps**. Props reads back the last value.
- Each change rebuilds that node's IR off the audio thread and crossfades across **one quantum**. Residual at a
  change: −31 to −49 dBc at the near ear, −41 to −51 dBc at the far ear. Compare −6 to −8 dBc for a relink
  click (Spike B3) and 0 for LSP band drags. Fine for a stepped "stage width" setting; not tested by ear.
- Only the changed node rebuilds. An EQ change elsewhere in the graph doesn't touch the spatializers
  (`node_control_changed()` is per node). The first change after instantiation rebuilt both nodes once.

## F6 — the HRIRs carry ~+14…17 dB of gain: calibrate a fixed makeup gain

| Config | Stereo pink −20 dBFS RMS in → out RMS | Out peak |
|---|---|---|
| as-is | **−3.1 dBFS** (+17 dB) | **+8.8 dBFS** |
| `normalize = true` (libmysofa `mysofa_loudness`) | −6.2 dBFS (+14 dB) | +5.7 dBFS |

(Identical L/R input sums coherently at both ears, which adds up to +6 dB at low frequencies.) In the F5 tone
test, a −12 dBFS tone already came out at +2.2 dBFS. `normalize` isn't enough. It also normalizes the *shared
cached* HRTF in place, so the first instance logs autogain 0.698 and the second logs 1.0.

→ The spatial stage needs a **calibrated `gain`** (a config key, so changing it means a reload) for each
bundled HRTF. Measure it once: match the loudness to bypass for stereo pink, and store it with the HRTF. The
limiter after the stage stays as the safety net.

## Verdict

- **`sofa` spatializer works**: it loads, the cues are correct, ~0.2 % CPU, ~23 MiB RAM, no added latency
  beyond the IR onset. Azimuth changes live.
- **Topology for omadsp:** fixed. Two spatializers + two mixers, always in the graph. "Spatial off" = bypass
  without a reload. The builtin `mixer` has `Gain 1…8` control ports (source read, 1.6.8). They are applied as
  a step per block with no smoothing, so mixing a dry path against the wet one through them would click.
  Switch them only inside a dip of the ramped playback volume (Spike A §4), same as LSP `enabled`.
- **HeSuVi:** don't ship the example. Its relative-path IR fails on first audio and silences everything
  behind the filter. If HeSuVi IRs are supported later, the user supplies the WAV and the daemon validates it
  and writes an absolute path.
- **Never call this Dolby/Atmos/DTS.** Wording: "virtual speakers (HRTF)".
