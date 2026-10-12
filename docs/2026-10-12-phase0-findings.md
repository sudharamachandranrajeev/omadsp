# Phase 0 — findings, decisions and the Phase 1 plan

Status: **awaiting your approval.** No production code exists yet. Everything below comes from the throwaway spikes in
`spikes/` (each has a `NOTES.md` with the method and raw numbers; A′, B, C, E and F also have a `run.sh` that
reproduces them, while A and D were driven step by step from the shell with the configs in their folders) and from
reading PipeWire 1.6.8,
WirePlumber 0.5.17, Quickshell 0.3.1 and Omarchy 4.0.4 source.

## 0. Summary

1. **The architecture holds, with one change of engine.** A WirePlumber smart filter, hosted in a separate process,
   processes audio correctly. Omarchy's "smart filter passes audio unprocessed" note describes a measurement trap,
   not a PipeWire bug (Spike A §1). The in-node alternative (A′) works but is worse on every axis that matters.
2. **PipeWire's builtin biquads click on every parameter change**: they reset their filter state (−16 dBc residual
   when dragged). The builtin `ramp` is broken (clamped to 0). So the EQ engine is **LSP `para_equalizer`**:
   click-free drags (−62 dBc), within 0.015 dB of RBJ, rate-aware. Gain changes go through the **ramped volume of
   the filter's playback node**. The `bq_raw` slot-pool question is moot.
3. **Reloading the filter costs ~0.1 s of silence plus a click**, whichever hosting model. So the graph has a **fixed
   topology**: every stage is always present and bypasses in place. Normal use never reloads.
4. **No DVC in v1.** Omarchy's volume keys, OSD and slider always act on the physical device, after the DSP. An
   automatic headroom preamp plus the limiter covers the same need.
5. **Overload LED from a daemon-side tap** on the filter output. The limiter's gain-reduction meter isn't
   exported by filter-chain 1.6.8, and Quickshell's peak monitor can only see pre-DSP audio.
6. **SOFA spatializer works** (correct ITD/ILD, ~0.2 % CPU, live azimuth). Its HRIRs need a calibrated makeup gain
   (+14–17 dB as shipped). The shipped HeSuVi example **silences all audio** when its IR file is missing. Missing
   files are only discovered when audio starts, so the daemon must validate files and watch for filter errors.

Measured budget: LSP EQ + limiter **0.27–0.66 % of one core** (0.99 % at quantum 256). True-peak oversampling
roughly doubles the limiter's share (1.1 % measured with the builtin-EQ chain). Added latency **5.0 ms**, or 7.6 ms
with true peak: the limiter's lookahead, nothing else. 0 xruns at quantum 1024 and 256, on a machine with **no
real-time scheduling**. The full MVP chain (x16 + x32 + TP limiter) is measured in Phase 1.

## 1. Versions (confirmed on this machine)

| Component | Version / state |
|---|---|
| Omarchy | 4.0.4-1 (`OMARCHY_PATH=/usr/share/omarchy`), Quickshell 0.3.1, Qt 6.11.2 |
| PipeWire / WirePlumber | 1.6.8 / 0.5.17 |
| filter-graph plugins | builtin, ebur128, ladspa, lv2, sofa (libmysofa 1.3.5) |
| LV2 | `lsp-plugins-lv2` 1.2.35 (installed for Phase 0). `x42-plugins-lv2` **not installed** |
| Rust | 1.98.1 (pacman). **cargo can't reach crates.io**: OpenSnitch has no rule for `/usr/bin/cargo` (it has rules for curl, uv, mise…) |
| Measurement | `spikes/.venv` (uv): numpy 2.5, scipy 1.18, soundfile |
| HRTF | SADIE II D2 (KEMAR), 48 kHz, 256-tap FIR SOFA, Apache-2.0, `spikes/F-spatial/assets/` (git-ignored) |
| Audio HW | ALC3204, 44.1/48 kHz only, `clock.allowed-rates [48000]`, quantum 1024. No BT controller, no USB DAC during Phase 0 |
| Scheduling | No RT anywhere: rtkit inactive, `ulimit -r` 0. PipeWire's own data loop is SCHED_OTHER |

## 2. Spike results

| Spike | Question | Answer | Notes |
|---|---|---|---|
| **A** smart filter | Does it process? Clicks? Latency/CPU? Picker? | **Processes** (+6.00 dB measured). Builtin biquads click, LSP doesn't. Ramped playback `volume` is click-free (needs `volumeRampStepSamples`). Filter-chain adds 0 samples; the limiter adds 240 (364 with true peak). Targetless filter follows default-device changes on its own (🔊 tested). **Listed in Omarchy's output picker.** WirePlumber persists filter volumes unless opted out. | `spikes/A-smart-filter/NOTES.md` |
| **A′** in-node graph | Is `audioconvert.filter-graph.N` a better host? | Works and runs pre-volume, but: the LV2 code runs **inside the PipeWire daemon**, graphs die with WirePlumber-owned device nodes, 4 KiB cap (oversize graphs silently dropped), `.disable` is a one-way latch, every replace is a hard cut, monitor is pre-graph. **Rejected as primary.** | `spikes/A2-in-node/NOTES.md` |
| **B** rates / slots | `bq_raw` pool? Rate changes? Bypass by relink? | `bq_raw` resets state like the other biquads → moot. LSP accurate at 48 and 44.1 kHz (≤ 0.008 dB). Retargeting via `filters` metadata works at runtime (~21 ms per move across rates). `filter.smart.disabled` relinks: **37–59 ms gap + click at every toggle** → not a bypass. | `spikes/B-rate/NOTES.md` |
| **C** hosting | In-process module vs child `pipewire -c` | Startup (~55 ms), reload gap (85–154 ms + click), RSS (~37 MiB) are a wash. **Crash isolation decides: separate process.** Host death → streams go straight to the device after 85 ms; respawn → back through the filter in 48 ms. | `spikes/C-hosting/NOTES.md` |
| **D** DVC | Map the filter sink volume into the graph? | `capture.volumes` works (no double gain), but nothing in Omarchy ever moves the filter's volume: keys, OSD and slider resolve to the physical sink. **No DVC in v1**; headroom preamp instead. **Naming rule** found: the playback node's name must start with the capture node's name. | `spikes/D-dvc/NOTES.md` |
| **E** meters | Read limiter GR? Quickshell peak monitor? | Filter-chain `Props` exports **input controls only** (378 of them; no `grlm`/`olm`). Quickshell `PwNodePeakMonitor` is diverted to the filter's pre-chain monitor whatever it targets. A **hand-linked capture on the filter output** separates limited from unlimited signals cleanly (74/74 vs 0/74 windows at the ceiling; true-peak mode needs a 0.5 dB margin). | `spikes/E-meters/NOTES.md` |
| **F** spatial | HeSuVi example, SOFA spatializer | HeSuVi: loads, then **fails on first audio and the stream stays on the dead filter → silence**. SOFA: correct cues (ITD 0.25–0.27 ms, ILD 3.6 dB), ~0.2 % CPU, +23 MiB, no added latency, live Azimuth (one-quantum crossfade, no gap), values survive suspend. HRIR gain +14–17 dB → calibrate. Plugins are instantiated only on first audio and re-instantiated after every idle suspend (~1.4 ms, nothing lost). | `spikes/F-spatial/NOTES.md` |

## 3. Decisions (ADRs)

Each ADR: context → decision → consequences. "§" refers to the spike notes.

### ADR-1 — Integration: targetless WirePlumber smart filter
- **Context.** PROMPT §5 chose a smart filter. F1 (Omarchy's failed attempt) and F4 (in-node graphs) put that in
  doubt. A §1 shows the smart filter processes, and that Omarchy measured with an autoconnected monitor recorder,
  which WirePlumber diverts to the filter's input. A′ works, but runs plugins inside the PipeWire daemon and loses
  the graph whenever WirePlumber recreates the device node.
- **Decision.** One smart filter. `media.class = Audio/Sink`, `filter.smart = true`, `filter.smart.name = "omadsp"`,
  **no `filter.smart.target`**, so it follows the default device (A §8). Never the default sink. Runtime retarget
  through the `filters` metadata only if a later feature needs it (B2).
- **Consequences.**
  - Streams that explicitly target another device are not processed (B2). This matches the Omarchy mixer's per-app
    moves.
  - Anything that records "the device monitor" through WirePlumber (OBS desktop audio, visualisers) gets **pre-DSP**
    audio (A §1). Documented, not fixed in v1.
  - The filter appears in Omarchy's output picker (A §9). See ADR-9.

### ADR-2 — Hosting: a separate host process supervised by `omadspd`
- **Context.** C measured no difference in startup, reload or memory. A plugin crash in-process would take down the
  daemon, its IPC and its state.
- **Decision.** `omadspd` writes `$XDG_RUNTIME_DIR/omadsp/host.conf` (managed header) and spawns
  `pipewire -c <that>`. This is the documented standalone-filter-chain path, the same shape as Omarchy's speaker
  tuning, and needs no FFI. The child gets `PR_SET_PDEATHSIG=SIGTERM`, so a dead daemon never leaves an unmanaged
  filter behind.
  - Respawn with backoff (250 ms → 5 s). After 5 crashes in 60 s, stay down and report `failed`.
  - The daemon controls the filter through its own PipeWire connection (`set_param` Props on the filter nodes).
- **Rejected.** Re-exec self + `pw_context_load_module` through `pipewire-sys`: workable (C, `inproc.c`), but no
  measured benefit for more `unsafe`.
- **Consequences.** A daemon or host crash means unprocessed audio after ~85 ms, never silence and never a wrong
  device (C3). A daemon restart costs two relinks (~85 + 48 ms).

### ADR-3 — EQ engine: LSP `para_equalizer` (APO mode), not builtin biquads
- **Context.** Builtin `bq_*`/`bq_raw` zero their state on every change (A §3, source). LSP: click-free drags,
  0.015 dB vs RBJ, bit-exact when bands are `Off` or `enabled = 0`, recomputes for the graph rate (B1).
- **Decision.**
  - User EQ (graphic up to 32 bands, or parametric up to 32 per channel): one `para_equalizer_x32_lr`.
  - Device correction (AutoEq, Phase 3, ≤ 14 filters) and tone (bass/treble shelves): bands 0–13 and 14–15 of one
    `para_equalizer_x16_lr`.
  - Filter mode APO (DR) everywhere (`fm* = 6`), which is what AutoEq/EqualizerAPO files assume.
  - Flat or unused bands are switched **`Off`**, never left at 0 dB (−95 dB residual otherwise).
  - LSP's `g_out` zippers (A §3), so it is never driven live.
- **Consequences.** `lsp-plugins-lv2` becomes a hard dependency. Poweramp's 64-band parametric is capped at 32 per
  channel in v1 (a second x32 would add CPU; revisit if asked). LTI stages commute in float, so their order in the
  graph doesn't change the response.

### ADR-4 — Gain staging, A/B and on/off without reloads
- **Context.** Reload ≈ 0.1 s + click (C). Relink bypass ≈ 37–59 ms gap + click (B3). Builtin `ramp` is broken (A §6).
  audioconvert's ramped `volume` on the playback node is click-free (A §4).
- **Decision.**
  - **Fixed topology:** `omadsp` (capture) → `pre` (x16: correction + tone) → `eq` (x32) → `lim` (limiter) →
    `omadsp_out` (playback). Later phases add stages that are always present and bypassed in place.
  - **Preamp / auto-headroom:** in-graph input gain at the head of the chain. Auto value = −(largest positive boost of
    the combined curve) + user offset, computed by the daemon. Changed only inside a **dip**. Live dragging of the
    preamp slider is measured in Phase 1 before its UI is designed.
  - **Dip:** ramp the playback node's `volume` to 0 over ~15 ms (step 4 samples), make the change, ramp back.
  - **A/B (hold):** dip → `enabled = 0` on every LSP stage → ramp up; release reverses it.
  - **DSP off:** dip, then `filter.smart.disabled = true` in the `filters` metadata (streams go straight to the device;
    one relink gap is acceptable for an explicit off). The host stays up, so "on" is quick. Bit-perfect mode (Phase 5)
    builds on the same switch.
  - **"omadsp level" trim** (optional): the ramped playback `volume`, post-limiter.
- **Consequences.** Nothing the user drags causes a reload. Reloads remain only for explicit mode changes in later
  phases (spatial mode, convolver IR swap), and the UI announces them.

### ADR-5 — Limiter: LSP `limiter_stereo`, last, always on while DSP is on
- **Context.** x42 is not installed, so it wasn't compared. LSP gives true peak (`ovs` 22 = True Peak/24 bit,
  +124 samples, ~1.1 % CPU) and a 5 ms lookahead (240 samples). Flat EQ + limiter below threshold is bit-identical
  with `ovs` off (A §2).
- **Decision.** LSP limiter, default ceiling −1.0 dBTP (PROMPT §8.5), `ovs` = True Peak/24 bit, lookahead 5 ms, ALR
  off. Phase 1 runs the PROMPT §10 null test with `ovs` on. If true-peak mode isn't transparent below the ceiling
  (≤ −120 dBFS), the default drops to sample-peak mode and true peak becomes an option.
- **Consequences.** 7.6 ms added latency (≤ 1 quantum budget at 1024). The overload LED threshold depends on this
  mode (ADR-7).

### ADR-6 — No Poweramp DVC in v1
- **Context and decision:** D. The user's volume (hardware mixer or PipeWire soft volume on the device) always applies
  after the chain, so the EQ and limiter run at full scale. Taking volume over would fight Omarchy's keys, slider and
  OSD.
- **Consequences.** `capture.volumes` is not used. Bluetooth absolute volume doesn't change this verdict (device
  volume, after the chain), but it's unverified (no adapter).

### ADR-7 — Meters and the overload LED
- **Decision.**
  - The daemon creates a passive capture stream (`node.autoconnect = false`, `node.passive = true`) and links it by
    hand to `omadsp_out:output_FL/FR`.
  - LED on when a 50 ms window's peak is within **0.5 dB of the ceiling**; hold 0.5 s.
  - It pushes state *changes* over IPC, and `meter` events (peak, ~20 Hz) only while a client subscribes.
  - The tap runs only while subscribed, or while the bar widget wants the LED (see risks).
- **Not available:** gain-reduction amount (E1). The panel shows "limiting", not "−x dB".
- **QML never meters** (E3 makes it pointless anyway).

### ADR-8 — Spatial (input for Phase 4)
- **Decision.**
  - `sofa`/`spatializer` pairs + builtin `mixer`, fixed topology.
  - A calibrated `gain` per bundled HRTF (F6), and check for "delay dropped" warnings when choosing one (F3).
  - Every file referenced by any graph (SOFA, IR WAV) is validated by the daemon and passed as an absolute path.
  - The HeSuVi example is not shipped. HeSuVi WAVs are user-supplied only.
- **Wording:** "virtual speakers (HRTF)". Never Dolby/Atmos/DTS.

### ADR-9 — Node naming, state and the picker problem
- **Decision.**
  - Capture `node.name = "omadsp"`, playback `node.name = "omadsp_out"`. The playback name starts with the capture
    name, so `omarchy-audio-output-sink` resolves through the filter and the volume keys still hit the device even if
    the user picks the filter (D).
  - Both nodes set `state.restore-props = false` and `state.restore-target = false` (A §5). Playback is
    `node.passive = true`.
  - The daemon watches `default.configured.audio.sink`. If it becomes `omadsp`, it puts the previous device back and
    notifies once ("omadsp isn't an output — it follows your device").
- **Also offer:** an upstream Omarchy issue proposing that the audio panel skip `filter.smart` nodes. I can draft the
  text. Nothing under `$OMARCHY_PATH` is edited.

### ADR-10 — Failure handling
- **Decision.**
  - Before every (re)load: validate graph inputs (files exist, readable, parse).
  - At runtime: watch both filter streams. On `error` state (F1's "can't start graph"), stop the host so streams fall
    through to the device, set state `failed`, and notify.
  - Reconnect to PipeWire with backoff after a server restart, then respawn the host.

### ADR-11 — IPC, CLI and plugin: as PROMPT §8.2–8.4
- No changes from PROMPT. One addition: the `meter` subscription also drives the tap (ADR-7).
- **Plugin id still open** (needed in Phase 2): `rajeevrama.omadsp` (matches your other plugins) or
  `io.github.<user>.omadsp`.

## 4. Revised risks

| Risk | Status → mitigation |
|---|---|
| Filter selected in Omarchy's picker becomes the configured default | Confirmed in code (A §9) → ADR-9 guard + naming; upstream issue |
| Device-monitor recorders get pre-DSP audio | Confirmed (A §1, E3) → document; possible "omadsp monitor" source later |
| A missing or broken plugin file silences all audio | Confirmed (F1) → ADR-10 validation + error watchdog |
| Reload = ~0.1 s gap + click | Confirmed (C) → fixed topology (ADR-4) |
| No RT scheduling on this machine | Confirmed; 0 xruns so far at 1024/256 → enabling rtkit is a sudo change, your call, optional |
| Plugins re-instantiated after every idle suspend | Confirmed, harmless so far (~1.4 ms, values kept) → re-check with big IRs (reverb, Phase 4) |
| PipeWire bugs we route around | biquad state reset, broken `ramp` hints, `filter-graph.disable` latch, silent 4 KiB drop, notify ports not exported. All avoided → optional upstream reports |
| True-peak limiter not transparent below ceiling | Unknown → Phase 1 null test; fallback sample-peak |
| pipewire-rs gaps (metadata writes, link creation, Props pods) | Unknown → Phase 1 step 2 checks first; raw `pipewire-sys` calls as fallback |
| cargo blocked | Confirmed → prerequisite below |
| BT, USB DAC, 96 kHz, BT absolute volume | Untested (no hardware) → deferred checks; Phase 3 exit needs the devices |
| WirePlumber state-stream fighting the daemon | Solved by the opt-out props (file unchanged across all later runs) |

## 5. What Phase 0 changed on your system

- **Packages you installed:** `lsp-plugins-lv2`, `rust`. A uv venv exists in `spikes/.venv`, git-ignored.
- **`~/.local/state/wireplumber/stream-properties`:** 7 entries from early spikes, written before the opt-out props
  existed. They are inert: those node names exist only while a spike runs.
  - Lines 20 and 22–27: `Audio/Sink:node.name:omadsp_spike_out`, plus `media.name` keys `omadsp spike EQ`,
    `omadsp spike EQ (LSP)` and `omadsp spike gain` (sink and output each).
  - Line 4 (`Quickshell Peak Detect`) was re-saved MONO → FL/FR by Spike E. Omarchy's own audio panel rewrites this
    entry too; all volumes are 1.0.
  - Lines 9 (`pw-record`) and 21 (`pw-cat`) may predate Phase 0 (the voice daemon uses `pw-record`). Leave them.
- **Removing those entries cleanly** needs WirePlumber stopped while the file is edited: it loads the file once and
  rewrites it whole from memory. I suggest doing it during Phase 1's restart test (step 9), which restarts WirePlumber
  anyway. You'd see the exact diff first.
- **`default-nodes`:** byte-identical to the pre-spike copy after the default-switch test.
- **Possible brief leak:** in the first A′ run, before the test-stream fix, a 1 kHz −20 dBFS test tone may have played
  on the speakers for a few seconds.
- **Journal:** the PipeWire daemon logged "Can't load filter-graph … Operation not permitted" during the A′ latch
  tests (private nodes, gone).
- Nothing written under `~/.config`, `$OMARCHY_PATH` or systemd. `pw-metadata` `default`/`filters`/`settings` match the
  baseline after every spike.

## 6. Prerequisites for Phase 1

| # | What | Who |
|---|---|---|
| R1 | Let cargo reach crates.io. Run `cargo search serde --limit 1` and choose **Allow always** for `/usr/bin/cargo` in the OpenSnitch prompt (or add it to netwatch's allow list). | you |
| R2 | Decide the plugin id (Phase 2, but it names the repo layout). | you |
| R3 | Optional: RT scheduling (rtkit), sudo. Not needed for Phase 1. | you, later |

## 7. Phase 1 plan — daemon core

Scope (PROMPT §9): model, graph generator, PipeWire control, smart-filter hosting, IPC, CLI, systemd unit, device
watcher, persistence. Exit: `omadsp status --json` works; CLI changes are audible and glitch-free; golden-file tests
pass; survives `systemctl --user restart pipewire wireplumber`.

### 7.1 Files

```
Cargo.toml                         workspace
crates/omadsp-core/                library, no PipeWire dependency, fully unit-testable
  src/model.rs                     Preset, Profile, State (+ schema version, migrations)
  src/graph.rs                     pure fn(&Preset, &Options) -> host.conf text (filter.graph SPA-JSON)
  src/controls.rs                  Preset -> list of ("node:port", value) for set_param; diffing
  src/dsp_math.rs                  RBJ biquads (LSP APO types), combined response at 256 log points, auto-headroom
  src/protocol.rs                  NDJSON v1 request/response/event types
  src/managed_file.rs              atomic writes; refuse symlinks and files without our header
crates/omadspd/                    daemon binary
  src/main.rs                      args (--target <node> for tests, --runtime-dir, --config-dir), logging
  src/pw.rs                        PipeWire client: registry, metadata (default, filters), node params, links
  src/host.rs                      host.conf writer + child supervisor (PDEATHSIG, backoff, error watchdog)
  src/apply.rs                     batching (≤ 1 set_param per node per 16 ms), dips, A/B, on/off
  src/device.rs                    active device = what omadsp_out links to; stable device key
  src/meter.rs                     hand-linked post-chain tap, LED logic
  src/ipc.rs                       Unix socket server in the PipeWire main loop
  src/store.rs                     presets/profiles/state on disk
crates/omadsp/                     CLI binary: toggle|on|off|ab|preset|band|preamp|tone|limiter|status --json|meter
packaging/systemd/omadspd.service  user unit (installed only after you approve the diff)
tests/golden/*.conf                graph generator golden files (UPDATE_GOLDEN=1 rewrites)
tests/integration/*.sh             end-to-end on the private sink, reusing spikes/lib
docs/2026-10-xx-phase1-summary.md  end-of-phase summary
```

**Crates:**
- `pipewire` 0.10 (+ `pipewire-sys` for any gaps);
- `serde`, `serde_json`;
- `clap` (CLI subcommands/help);
- `tracing` + `tracing-subscriber` (stderr → journal);
- `libc` (prctl, signals);
- `anyhow`.

**Not used:** `tokio` (the PipeWire main loop is the only event loop; the socket fds are added to it, so a single
thread, no locks), `notify` (Phase 3), `toml` (presets are JSON per PROMPT §8.5), `uuid` (request ids come from
clients).

**Managed-file header in JSON:** JSON has no comments, so files the daemon writes start with
`"//": "Managed by omadsp — do not edit"`. The `.conf` and `.service` files get the real `#` comment.

**Locations:** `~/.config/omadsp/{presets/*.json,profiles.json}`, `~/.local/state/omadsp/state.json`,
`$XDG_RUNTIME_DIR/omadsp/{omadsp.sock,host.conf}`. The first two are outside the repo, so their first creation
happens only after you OK it.

### 7.2 Steps (each a small commit)

1. Workspace skeleton, `omadsp-core` model + protocol types, unit tests.
2. **pipewire-rs check:** a tiny binary that sets a `filters` metadata key, sets Props params on a node, creates a
   passive link, and reads link/default state on the private sink. If any piece is missing, wrap it with
   `pipewire-sys` here.
3. `dsp_math`: RBJ (peak, shelves, LP/HP, BP, notch), combined curve, headroom. Unit tests against reference values
   and against Spike A/B measurements (≤ 0.015 dB).
4. `graph` + `controls` + golden files. Fixed topology `pre` (x16) → `eq` (x32) → `lim`, state opt-out props,
   naming rule.
5. `host`: write conf, spawn, supervise, error watchdog, PDEATHSIG.
6. `pw` + `apply` + `device`: filter discovery, batched set_param, dips (ramped playback volume), A/B, on/off via
   `filter.smart.disabled`, picker guard, device key.
7. `store` + `ipc` + CLI. Default first-run state: **DSP off**, preset "Flat".
8. `meter` tap + LED events.
9. Tests:

| Test | Where | Pass |
|---|---|---|
| Unit + golden | cargo test | all green |
| Response vs `dsp_math` curve (graphic V, 32-band, shelves) | private sink, 48 & 44.1 kHz | ±0.1 dB |
| Null: DSP on, flat, limiter on vs `filter.smart.disabled` | private sink | ≤ −120 dBFS (decides ADR-5's `ovs` default) |
| Glitch: `omadsp band` driven 20×/s for 60 s; A/B toggles; preamp changes | private sink, quantum 1024 and 256 | 0 ERR, clicks < −60 dBc |
| Preamp drag behaviour (`g_in` live vs dip) | private sink | picks the preamp UI strategy |
| Budget | `pw-top` + `/proc` | ≤ 2 % core, latency = limiter lookahead |
| Kill `omadspd`; kill the host | private sink | audio continues; restart restores state |
| 🔊 Real device: `omadsp on`, band changes, A/B, default switch | speakers, low level | audible, no clicks; filter never default (I'll ask first) |
| ⚠ `systemctl --user restart pipewire wireplumber` | whole system | daemon recovers; I'll ask first, because it interrupts all audio including the voice-control recorder; stream-properties cleanup (§5) at the same moment if you want it |

10. systemd unit: show the diff for `~/.config/systemd/user/omadspd.service` (ExecStart → the repo's
    `target/release/omadspd` during development) and enable it only with your OK.
11. Phase summary doc + file list. Stop.

### 7.3 Phase 1 risks

- pipewire-rs API gaps → step 2 settles it before anything depends on it.
- Server restart behaviour of a `pipewire -c` host (does it exit or hang?) → handled by the supervisor either way;
  measured in step 9.
- Real-device tests touch your speakers and the default device → only with your go-ahead at that step.

## 8. Files created in Phase 0

All on branch `phase0-spikes` (plan on `main`):

- `docs/2026-10-12-phase0-plan.md`, `docs/2026-10-12-phase0-findings.md`
- `.gitignore` (`PROMPT.md` is your file, committed unchanged)
- `spikes/lib/{common.sh,measure.py,signals.py,mkhost.py,base-host.conf,rbj.py}`
- `spikes/A-smart-filter/{filter.conf.in,lsp-peq.conf.in,ramp-gain.conf.in,NOTES.md}`
- `spikes/A2-in-node/{run.sh,NOTES.md}`
- `spikes/B-rate/{run.sh,NOTES.md}`
- `spikes/C-hosting/{inproc.c,run.sh,NOTES.md}`
- `spikes/D-dvc/{dvc.conf.in,NOTES.md}`
- `spikes/E-meters/{meter.conf.in,qs/shell.qml,run.sh,NOTES.md}`
- `spikes/F-spatial/{sofa.conf.in,run.sh,NOTES.md}`
- Git-ignored: `spikes/.venv/`, `spikes/F-spatial/assets/D2_48K_24bit_256tap_FIR_SOFA.sofa`, all `*.wav`
- Outside the repo: only the WirePlumber state entries in §5, and runtime scratch in `$XDG_RUNTIME_DIR/omadsp-spike/`
  (deleted at next reboot)
