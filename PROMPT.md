# Claude Code build prompt — native system-wide DSP for Omarchy (Poweramp-style)

> **How to use:** create an empty folder (e.g. `~/Projects/omadsp`), save this file there as `PROMPT.md`, run `claude` in that folder and say:
> *"Read PROMPT.md and start Phase 0. Do not write production code until I approve the Phase 0 report."*
>
> Working name used below: **omadsp** (daemon `omadspd`, CLI `omadsp`, plugin id `io.github.<github-user>.omadsp`). Rename freely — but third-party plugin ids must **not** start with `omarchy.`.

---

## 1. Role and mission

You are a senior Linux audio + Qt/QML engineer. Build a **native, system-wide audio DSP for Omarchy 4 ("Quattro")** whose UI and feature set are inspired by **Poweramp / Poweramp Equalizer** (Android), controlled from the **Omarchy bar** (Quickshell plugin), with a headless daemon doing the real work on **PipeWire's own filter-chain** — no EasyEffects, no JamesDSP, no extra audio server.

Goals, in priority order:
1. **Transparent and safe:** with DSP flat or off, output is bit-identical (or < −120 dB difference) to no DSP; never clips; never hijacks app routing; audio keeps playing if the daemon dies.
2. **Native:** PipeWire filter-chain + WirePlumber smart filters for audio; Omarchy plugin (QML, Omarchy `Style` theme tokens) for UI; systemd user service for the daemon.
3. **Poweramp-grade UX:** fast, tactile EQ (graphic + parametric), tone, presets per device, AutoEq library, overload indicator, A/B bypass.
4. **Quality output controls:** sample-rate policy, bit depth, resampler, dither, Bluetooth codec, bit-perfect mode.
5. **Spatial:** headphone virtual surround (HRIR/SOFA), stereo externalisation, later head-tracking.

## 2. Working agreement (follow strictly)

- **Plan first, then wait.** At the start of every phase, write the plan (files to create/change, commands to run, risks) and **stop for my approval**. After each phase, summarise what was done, what changed, what's next.
- **Protect my system.** Before deleting, overwriting or renaming any existing file — and before writing **anything outside this repo** (e.g. `~/.config/pipewire`, `~/.config/wireplumber`, `~/.config/omarchy`, systemd units) — show me the exact change (path + diff) and wait for confirmation. Never edit packaged Omarchy files under `$OMARCHY_PATH`. Never use `sudo` without asking.
- **Every file you write outside the repo** must carry a header comment `# Managed by omadsp — do not edit` and the daemon must refuse to overwrite a file it doesn't own or a symlink.
- **Verify, don't assume.** Anything marked **VERIFY** below came from research but was not confirmed on this machine — confirm it from source/docs/experiments before building on it. Never invent Omarchy manifest fields; read `$OMARCHY_PATH/shell/services/PluginRegistry.qml` for the real schema.
- **Docs naming:** design notes / ADRs go in `docs/` named `YYYY-MM-DD-descriptive-name.md`.
- **End of each phase:** list every file created or modified, with its location.
- Commit in small, reviewable steps with clear messages.

## 3. Target environment (confirm in Phase 0)

| Component | Expected | Check |
|---|---|---|
| Distro | Omarchy 4.x (Arch, Hyprland) — shell is a single Quickshell process with a plugin system; Waybar is gone | `omarchy version` (or equivalent), `echo $OMARCHY_PATH` |
| Shell | Quickshell 0.3.x hosting `omarchy-shell` | `qs --version` |
| Audio | PipeWire **≥ 1.6.6** (filter-graph volume fixes landed in 1.6.6), WirePlumber 0.5.x | `pipewire --version`, `wireplumber --version` |
| SOFA support | Arch builds `pipewire` against libmysofa, so `type = sofa` should be available | load a test spatializer graph |
| LV2 plugins | `lsp-plugins-lv2` (limiter, compressor, loudness compensator), optional `x42-plugins-lv2` | `pacman -Q`, `lv2ls \| grep -i -E 'lsp\|x42'` |
| Language/toolchain | Rust stable (recommended for daemon) | `rustc --version` |

## 4. What we're building — feature map (Poweramp → omadsp)

| Poweramp / Poweramp Equalizer feature | omadsp implementation |
|---|---|
| Graphic EQ, fixed 5/8/10/12/15/16/24/31/32 or custom 5–32 bands, configurable start/end freq, ±15 dB | Pre-allocated pool of 32 `bq_peaking` per channel, log-spaced; unused bands at 0 dB (unity) |
| Parametric EQ (Poweramp now advertises 64 bands): per-band type (LP/HP/LS/HS/BP/Peak), channel (L/R/both), freq, gain, Q, band colour; bands added/removed by user | Slot pool (see spike B); response curve with draggable nodes; per-band colour stored in preset |
| Preamp | `linear` builtin (`Mult`) at chain head, plus **auto-headroom** = −(max positive boost of combined curve) |
| Bass / Treble (tone) with stored-in-preset values | `bq_lowshelf` / `bq_highshelf` with adjustable corner freq |
| Limiter + compressor | LSP limiter (or x42 digital peak limiter) last in chain — confirm which offers true-peak/oversampling via `lv2info`; optional LSP compressor |
| Balance, mono mix, stereo expansion | Native mixer matrix (M/S width, balance, mono) built from `mixer`/`invert`/`copy` builtins |
| Reverb | `convolver` with bundled room/hall IRs + wet/dry mixer |
| AutoEq presets, preinstalled, searchable; **suggested when a device connects** | AutoEq index; `param_eq` can load AutoEq/Squiglink ParametricEQ.txt directly; suggest match on new device |
| Presets per device/output, preset lock, autosave, search/filter, import/export | Device profiles keyed by stable device identity; lock + autosave flags; JSON + AutoEq `.txt` + EqualizerAPO import |
| **DVC (Direct Volume Control)** — volume applied inside the DSP for EQ headroom; "No-DVC headroom gain" (default −6 dB) when not | Map the virtual sink's volume to an in-graph gain via `capture.volumes` (spike D); pin hardware volume to a set point; otherwise apply headroom gain |
| Limiter "overload" highlight on the button | Bar icon turns amber/red when limiter engages or peak ≥ ~−0.1 dBFS |
| Per-output options, allowed sample rates, bit-perfect modes, Bluetooth codec matching | "Output" tab (section 8.6) |
| Visualisations / spectrum | Stretch: spectrum from a peak/FFT tap; not MVP |

Not applicable system-wide: tempo, gapless, ReplayGain tags (replace with an EBU R128 "volume leveler").

## 5. Architecture (decided — challenge it in Phase 0 only with evidence)

```
 apps ──► [omadsp  Audio/Sink main node] ──► filter-chain graph ──► [stream node] ──► default device
           (WirePlumber smart filter: apps keep          (float32, RT thread)          (ALSA / USB DAC / BT)
            targeting the real device; filter is
            inserted transparently)
                      ▲ live control: node set_param(Props{params=[ "node:Port" value … ]})
                      │ topology change: regenerate graph + reload instance (debounced)
 ┌────────────────────┴─────────────────────────────────────────────────┐
 │ omadspd (Rust, systemd --user)                                       │
 │  • state + presets + device profiles (single source of truth)        │
 │  • graph generator: pure fn(Preset, DeviceCaps, Rate) -> filter.graph │
 │  • PipeWire client: watches default sink, devices, rate; sets params │
 │  • RBJ biquad math → combined response curve for the UI              │
 │  • AutoEq index + matcher                                            │
 │  • IPC: NDJSON over $XDG_RUNTIME_DIR/omadsp.sock                     │
 └───────────────▲───────────────────────────────▲──────────────────────┘
                 │                               │
     Omarchy plugin (QML in omarchy-shell)    omadsp CLI (keybinds, scripts)
     BarWidget.qml + Panel.qml (+ Service.qml)
```

Why these choices:
- **Smart filter, not a "default sink" virtual device.** WirePlumber links streams headed for the target device through the filter chain transparently, and can be disabled/retargeted at runtime via the `filters` metadata. This avoids the JamesDSP failure mode reported *on Omarchy* where its virtual sink grabs every new stream regardless of the chosen default.
- **filter-chain builtins** cover biquads (live Freq/Q/Gain), parametric EQ with AutoEq file loading, convolver, delay, mixer, EBU R128 loudness + `lufs2gain`, SOFA spatializer, dcblock, noisegate, ramp — plus LV2/LADSPA hosting for the limiter/compressor.
- **Daemon owns state**, QML is a thin view (same pattern as other Omarchy plugins that bridge to a helper over NDJSON). Omarchy's plugin installer never runs install hooks or sudo, so the daemon ships as a separate package (PKGBUILD) and the plugin documents that dependency.

## 6. Signal chain (stereo, explicit per-channel nodes — no implicit graph duplication)

| # | Stage | Nodes (per channel unless noted) | Live? |
|---|---|---|---|
| 1 | Input trim / auto-headroom | `linear` (`Mult`) | yes |
| 2 | Device correction (AutoEq layer, per device) | `param_eq` from file **or** biquad slots | file: reload; slots: yes |
| 3 | User EQ — graphic **or** parametric (per preset) | 32 `bq_peaking` slots / parametric slot pool | yes (type change: spike B) |
| 4 | Tone | `bq_lowshelf` (bass), `bq_highshelf` (treble) | yes |
| 5 | Loudness compensation (optional) | LSP loudness compensator (LV2/LADSPA — **VERIFY** URI/ports) | yes |
| 6 | Stereo matrix (balance, width, mono) | `mixer` / `invert` / `copy` (stereo, cross-channel) | yes |
| 7 | Crossfeed (headphones) | native bs2b-style: `bq_lowpass` + `delay` + `mixer`; or a bs2b LV2 if packaged (**VERIFY**) | yes |
| 8 | Spatial (off / stereo externalise / virtual 7.1) | `sofa` spatializer nodes or HRIR `convolver` set | angles yes; mode: reload |
| 9 | Reverb | `convolver` (IR) + wet/dry `mixer` | wet/dry yes |
| 10 | Volume leveler (optional) | `ebur128` → `lufs2gain` → `linear` | yes |
| 11 | DSP volume ("DVC") | target of `capture.volumes` (spike D) | yes |
| 12 | Peak limiter (always last, always on when DSP on) | LSP or x42 limiter LV2 (URI from `lv2ls`; prefer true-peak/oversampled mode) | yes |
| 13 | A/B bypass | dry/wet `mixer` driven by `ramp` (glitch-free) | yes |

Full bypass / bit-perfect: disable the smart filter via `pw-metadata -n filters <main-node-id> filter.smart.disabled true Spa:String:JSON` so streams go straight to the device.

## 7. Phase 0 — recon + spikes (report, then STOP)

Read before writing anything:
1. Omarchy shell source: `$OMARCHY_PATH/shell/README.md`, `services/PluginRegistry.qml` (manifest schema: kinds, `service` entry point, settings `defaults`/`schema`, keybindings if any), `plugins/panels/audio/` (built-in `omarchy.audio`: volume, output picker, per-app mixer — learn how it lists sinks so our filter node doesn't confuse it), `plugins/bar/` clock example, `plugins/osd/` (can we raise an OSD on preset change?).
2. Run `omarchy plugin clone omarchy.clock --edit` in a scratch copy to see a working bar-widget + panel; then remove the clone (ask first).
3. `pw-dump`, `wpctl status`, `pw-top` on the idle system; note default sink, its `EnumFormat` rates/formats, Bluetooth devices.

Spikes (throwaway code in `spikes/`, each with a short findings note):
- **A — Smart filter EQ:** a hand-written filter-chain (10× `bq_peaking` per channel + LSP limiter) as a smart filter. Prove: apps playing to the default device are routed through it; switching default device follows; `pw-cli s <id> Props '{ params = [ "eqL_3:Gain" 6.0 "eqR_3:Gain" 6.0 ] }'` changes sound live with no clicks while dragging (20 updates/s); measure added latency/CPU in `pw-top`; confirm it is **not** offered as a normal output in Omarchy's audio picker (or decide how to hide it).
- **B — Parametric slots:** can `bq_raw` coefficients (`b0..a2`) be written at runtime and do they survive a graph-rate change (44.1 ↔ 48 ↔ 96 kHz)? If yes → parametric pool of 64 `bq_raw` slots, daemon computes RBJ coefficients for any type and recomputes on rate change. If no → per-type slots, or debounced graph rebuild on *type* change only (freq/gain/Q stay live). Measure the audible gap of a rebuild.
- **C — Hosting:** compare (1) loading `libpipewire-module-filter-chain` inside the daemon's own PipeWire context (check whether `pipewire-rs` exposes module loading — **VERIFY**; otherwise thin C FFI) vs (2) supervising a child `pipewire -c <generated>.conf` (documented way to run a filter-chain standalone). Pick based on rebuild speed, RT priority (rtkit), crash isolation.
- **D — DVC:** map the filter sink's volume to an in-graph control with `capture.volumes = [ { control = "<node>:<port>" … } ]`; verify interplay with Omarchy's volume slider/OSD and Bluetooth absolute volume; decide "DVC on" semantics (hardware volume pinned to a user set point, software volume in float inside DSP).
- **E — Meters:** can we read LV2 output control ports (limiter gain reduction) via node params? If not, use Quickshell `PwNodePeakMonitor` on the filter's output stream for the overload LED.
- **F — Spatial:** load `/usr/share/pipewire/filter-chain/sink-virtual-surround-7.1-hesuvi.conf` (**VERIFY** path) and a SOFA spatializer graph (e.g. stereo → virtual speakers at ±30°); check CPU and latency; confirm azimuth/elevation can change live.

**Phase 0 deliverable:** `docs/YYYY-MM-DD-phase0-findings.md` with versions, spike results, decisions (ADR style), revised risks, and the exact plan for Phase 1. Then stop.

## 8. Component specifications

### 8.1 Daemon `omadspd` (Rust)
- Crates (suggested): `pipewire` (pipewire-rs), `tokio`, `serde`/`serde_json`, `toml`, `notify` (file watch), `tracing`. Justify any others.
- Modules: `model` (preset/profile types + migrations), `graph` (pure generator → SPA-JSON string; golden-file tests), `pw` (PipeWire client: registry watch, default-sink metadata, device identity, rate tracking, `set_param`), `dsp_math` (RBJ biquads, combined magnitude response at 256 log-spaced points, headroom calc), `autoeq` (index + parser for `ParametricEQ.txt`: `Preamp:` and `Filter N: ON PK|LSC|HSC Fc … Gain … Q …`), `ipc` (NDJSON server), `profiles` (device → preset map).
- Batching: coalesce UI changes to ≤ 1 `set_param` per node per ~16 ms; one Props call can carry many `params` pairs.
- Device identity: prefer stable keys (`device.bus-path` / `api.bluez5.address` / USB vendor:product + serial / `node.name`), never node ids.
- Persist: config `~/.config/omadsp/` (presets, profiles — human-readable JSON/TOML), state `~/.local/state/omadsp/`, socket + generated graphs `$XDG_RUNTIME_DIR/omadsp/`.
- Resilience: reconnect on PipeWire/WirePlumber restart; if the daemon exits, audio must continue (filter removed → WirePlumber links streams to the device directly — **VERIFY** with the chosen hosting model).
- systemd user unit `omadspd.service` (ordered after `pipewire.service` and `wireplumber.service`, restarts on failure).

### 8.2 IPC protocol (NDJSON v1 over Unix socket)
Request: `{"v":1,"id":"<uuid>","type":"band.set","payload":{...}}` → response echoes `id` with `ok`/`error`. Events: `{"v":1,"event":"state.changed","payload":{...}}`.
Types (initial): `state.get`, `dsp.setEnabled`, `dsp.abHold`, `preset.apply|save|rename|delete|lock|import|export`, `eq.setMode`, `band.set|add|remove|reorder`, `tone.set`, `preamp.set`, `stereo.set`, `crossfeed.set`, `spatial.set`, `reverb.set`, `leveler.set`, `limiter.set`, `output.set`, `profile.assign|clear`, `autoeq.search|apply`. Events: `state.changed`, `device.changed`, `curve.updated` (array of dB values), `meter` (peak/GR, ~20 Hz, **only while a panel subscribes**), `autoeq.suggestion`.

### 8.3 CLI `omadsp`
`omadsp toggle | on | off | ab | preset next|prev|<name> | band <i> <gain> | status --json | autoeq search <query> | autoeq apply <id>`. Suitable for Hyprland keybinds (`~/.config/hypr/bindings.conf` — ask before editing).

### 8.4 Omarchy plugin (Quickshell / QML)
- Repo layout = plugin root: `manifest.json`, `BarWidget.qml`, `Panel.qml`, optional `Service.qml`, `components/`, `README.md`, `LICENSE`, `preview.png`. **No symlinks** in the plugin folder.
- Manifest (schemaVersion 1): `kinds: ["bar-widget"]` (+ `"service"` if `PluginRegistry.qml` allows multi-kind and `shell.serviceFor(id)` is the right access path — **VERIFY**), `entryPoints`, `barWidget.displayName/category/allowMultiple:false/defaultSection:"right"`, `barWidget.defaults`/`schema` only for genuinely user-tweakable widget settings.
- Follow the built-in pattern: `BarWidget` forwards `open()/close()/toggle()/closeForPopoutSwitch()` to a `Panel` loaded via `Loader`; `KeyboardPanel` anchored to the bar button; `PanelKeyCatcher` for Escape/Tab; `Style.*` tokens and bar colours so it follows the Omarchy theme switcher and all four bar edges.
- IPC from QML: `Quickshell.Io` `Socket` (persistent NDJSON connection, auto-reconnect) — no polling `Process` spawns; `Process` only for one-shots. One connection shared via the service singleton.
- Never start a second Quickshell process; plugins run unsandboxed inside the shell — keep QML light, no heavy JS loops.

**Bar widget**
- Icon states: off (dim) · on · A/B-bypassed (outline) · overload (amber → red).
- Left click: open panel. Right click: DSP on/off. Middle click: hold-to-A/B. Scroll: preamp ±0.5 dB (configurable: or cycle presets).
- Tooltip: preset name · device · "limiter active" if engaged.

**Panel (Poweramp-inspired layout)**
- Header row: power switch · preset chip (tap → preset sheet) · device name + type icon · overload LED · lock icon.
- Tabs: **EQ** · **Tone** · **Space** · **Effects** · **Output** · **Presets**.
  - *EQ — graphic:* vertical band sliders with a smooth response curve overlay; tap value to type; double-tap resets to 0; band count selector (5…32) and freq range; preamp slider; "Auto headroom" toggle.
  - *EQ — parametric:* large response graph (log freq 20 Hz–20 kHz, ±15 dB grid) with draggable coloured nodes (drag = freq/gain, scroll/pinch = Q); band list below: type · channel L/R/Both · freq · gain · Q · colour · enable; add/remove/reorder.
  - *Device correction strip* (above EQ): AutoEq profile name, on/off, "Change…".
  - *Tone:* Bass and Treble knobs (± dB) with corner-frequency sub-knobs.
  - *Space:* Balance knob, Width (0–200 %), Mono, Crossfeed (off/low/high/custom), Spatial mode (Off / Externalise / Virtual 7.1) + HRTF picker (bundled + user SOFA/HeSuVi files).
  - *Effects:* Reverb (preset IR + wet), Volume leveler (target LUFS), Compressor, Limiter ceiling (dBTP) + release.
  - *Output:* section 8.6.
  - *Presets:* search/filter; groups Built-in · User · AutoEq; actions apply/save-as/rename/delete/lock/export; "Assign to this device"; autosave toggle.
- AutoEq suggestion: when a new output appears with no profile, show a non-blocking banner "Found AutoEq profile for <model> — Apply?" (and a desktop notification).
- Performance: response curve drawn with a single `Shape`/`Canvas`; meters update only while panel is open.

### 8.5 Preset / profile model (example)
```json
{
  "schema": 1,
  "name": "Warm V",
  "locked": false,
  "eq": { "mode": "graphic", "bands": 10, "range": [31, 16000], "gains": [3,2,1,0,-1,-1,0,1,2,3] },
  "parametric": [ { "type": "peak", "ch": "both", "f": 120, "g": 2.5, "q": 0.9, "color": "#e07a5f", "on": true } ],
  "preamp": { "db": -3.0, "auto": true },
  "tone": { "bass": { "db": 2, "f": 105 }, "treble": { "db": 1, "f": 8000 } },
  "stereo": { "balance": 0, "width": 1.0, "mono": false },
  "crossfeed": { "mode": "off" },
  "spatial": { "mode": "off", "hrtf": null },
  "reverb": { "ir": null, "wet": 0 },
  "leveler": { "on": false, "target_lufs": -18 },
  "limiter": { "ceiling_dbtp": -1.0, "release_ms": 50 }
}
```
Profiles: `{ "device_key": "...", "label": "Sony WH-1000XM5 (BT)", "correction": "autoeq:<id>|null", "preset": "Warm V", "dvc": true, "output": { ... } }`.

### 8.6 Output-quality controls (Output tab)
- **Sample-rate policy:** Auto (graph switches among allowed rates when idle — PipeWire's `default.clock.allowed-rates`, off by default upstream because of kernel/Bluetooth issues; show that caveat) · Fixed rate (`pw-metadata -n settings 0 clock.force-rate <rate>`) · Reset. Offer only rates the current device advertises in `EnumFormat`.
- **Bit depth** (local ALSA only): WirePlumber rule forcing `audio.format` S16LE/S24LE/S32LE for the current device (requires WirePlumber restart — confirm with user).
- **Resampler quality:** `resample.quality` (PipeWire default 4; **VERIFY** valid range) and window (`resample.window` exp/blackman/kaiser in 1.6).
- **Dither:** `dither.method` none / rectangular / triangular / triangular-hf / wannamaker3 / shaped5, `dither.noise`.
- **Bluetooth:** show active codec; codec picker from the device's supported list; LDAC quality `bluez5.a2dp.ldac.quality` auto/hq/sq/mq.
- **Bit-perfect mode:** one switch that (a) disables the smart filter, (b) sets stream/device volume to 100 % or warns, (c) optionally locks the rate to the playing stream's rate when a single stream is active. Show a "signal path" readout (source rate/format → graph rate → device format → codec) like Poweramp's path info.
- All config written as owned drop-ins under `~/.config/pipewire/` or `~/.config/wireplumber/` — **ask before writing**; prefer runtime metadata where possible.

### 8.7 Spatial — scope and honesty
- There is **no FOSS real-time Dolby Atmos object decoder** that plugs into PipeWire. Do not claim "Atmos". Channel-based 5.1/7.1 from players/games is supported via a multichannel virtual sink → HRIR/SOFA binaural render ("virtual surround").
- Stereo externalisation: two virtual speakers at ±30° (SOFA spatializer) + optional early-reflection IR.
- Bundled HRTFs only if their licence allows redistribution (**VERIFY** each); otherwise let users add SOFA/HeSuVi WAV files.
- Stretch: head-tracking (OSC/UDP from opentrack → daemon → spatializer azimuth live), FIR room correction (load a REW/AutoEq FIR WAV into `convolver`), stereo→5.1 upmix.

## 9. Phase plan (each phase: plan → my approval → build → test → summary)

| Phase | Scope | Exit criteria |
|---|---|---|
| 0 | Recon + spikes A–F (section 7) | Findings doc approved |
| 1 | Daemon core: model, graph generator, PipeWire control, smart-filter hosting, IPC, CLI, systemd unit, device watcher, persistence | `omadsp status --json` works; CLI changes are audible and glitch-free; golden-file tests pass; survives `systemctl --user restart pipewire wireplumber` |
| 2 | Omarchy plugin MVP: bar widget + panel (power, presets, graphic EQ, preamp/auto-headroom, tone, limiter, overload LED, A/B) | `omarchy plugin validate` + `qmllint -I "$OMARCHY_PATH/shell"` clean; works after `omarchy restart shell`, theme switch, bar on all 4 edges |
| 3 | Parametric EQ + response graph; AutoEq index + device suggestion; per-device profiles; lock/autosave; import/export | Plug/unplug USB DAC and BT headset → correct profile < 500 ms; AutoEq import matches reference curve ±0.1 dB |
| 4 | Space + Effects: balance, width, mono, crossfeed, spatial modes, reverb, leveler, compressor | CPU budget met; spatial modes switch without breaking routing |
| 5 | Output tab: rate policy, bit depth, resampler, dither, BT codec/LDAC, bit-perfect mode, signal-path readout | Bit-perfect null test passes; every setting reversible from the UI |
| 6 | Packaging + polish: PKGBUILD (daemon + bundled assets), plugin repo per Omarchy marketplace publishing guide, README (deps, services, privilege boundaries), screenshots | Clean install on fresh Omarchy VM using only README steps |

## 10. Testing and acceptance
- **Null test:** DSP on with everything flat (incl. limiter) vs smart filter disabled → capture both via `pw-record` from the device monitor; difference ≤ −120 dBFS (limiter must be transparent below ceiling).
- **Response test:** pink noise/log sweep through the chain; measured magnitude vs daemon's computed curve within ±0.1 dB (20 Hz–20 kHz) at 44.1/48/96 kHz.
- **Glitch test:** slider drag at 20 Hz for 60 s → no xruns (`pw-top` ERR column), no clicks; quantum 256 and 1024.
- **Routing test:** default device change, per-app move in Omarchy mixer, BT connect/disconnect, USB DAC hotplug, suspend/resume — no stream ever lands on the wrong device; filter never becomes the "default sink".
- **Resilience:** kill `omadspd` → audio continues; restart → state restored.
- **Budget:** ≤ 2 % of one core at 48 kHz/quantum 1024 for MVP chain; added latency ≤ 1 quantum (convolver/spatial modes documented separately).
- Unit tests: RBJ math vs reference values, AutoEq parser, graph generator golden files, protocol schema.

## 11. Guardrails / do-nots
- Don't depend on EasyEffects/JamesDSP/CamillaDSP at runtime (fine to read their code for ideas; respect licences).
- Don't set the filter as the default sink; don't edit `$OMARCHY_PATH`; don't add unsupported manifest fields; no symlinks in the plugin folder; no `omarchy.*` ids.
- Don't poll PipeWire from QML; don't spawn processes per frame.
- Don't rely on `pw-dump` to read back changed control values (a past regression showed stale values) — the daemon is the source of truth.
- FFmpeg filtergraph nodes are not real-time safe — only use with `node.async = true`, never in the default chain.
- Never claim Dolby/Atmos/DTS branding in UI or docs.

## 12. Research brief (verified 12 Oct 2026 — re-check versions on the machine)

**Omarchy 4 / Quickshell**
- Omarchy 4.0 "Quattro" rebuilt the desktop shell in Quickshell; bar, launcher, menus, notifications, OSDs, panels, lock screen and polkit agent run as plugins in one process; third-party bar widgets/whole bars install via `omarchy plugin add <git-url>`. — https://github.com/basecamp/omarchy/releases/tag/v4.0.0 (repo now at https://github.com/omacom/omarchy, branch `quattro`)
- Plugin kinds: `bar-widget`, `panel`, `overlay`, `menu`, `service` (headless singleton), `bar`. Plugins live in `~/.config/omarchy/plugins/<id>/`; saving files hot-reloads; IPC: `omarchy-shell shell summon|hide|toggle|call|rescanPlugins|listPlugins`; layout + inline settings persist in `~/.config/omarchy/shell.json`. — https://github.com/omacom/omarchy/blob/quattro/shell/README.md
- Dev guide (clone a built-in, manifest example, BarWidget/Panel pattern, `omarchy plugin validate`, `qmllint -I "$OMARCHY_PATH/shell"`, logs `qs log -p "$OMARCHY_PATH/shell" --tail 100`, ids can't use `omarchy.*`, no symlinks). — https://plugins.omarchy.org/develop.html · publishing: https://plugins.omarchy.org/publish.html
- Built-in audio widget `omarchy.audio`: "Volume slider, output picker, per-app mixer". — `$OMARCHY_PATH/shell/plugins/panels/audio/manifest.json`
- Prior art: **Audio Patchbay** bar plugin (device-aware sample-rate presets via `pw-metadata` `clock.force-rate`/`clock.force-quantum`; bit depth via an owned WirePlumber rule + restart; never sudo). — https://github.com/devfrp/omarchy-audio-pro
- Service + NDJSON helper pattern (BarWidget gets service via `shell.serviceFor(pluginId)`). — https://raw.githubusercontent.com/digitalpals/omarchy-t3code/main/ARCHITECTURE.md
- Quickshell: `Quickshell.Services.Pipewire` (nodes, default sink, volumes) and `PwNodePeakMonitor` (per-channel peaks 0–1); `Quickshell.Io` has `Socket`, `Process`, `SplitParser`, `StdioCollector`, `IpcHandler`. — https://quickshell.org/docs/master/types/Quickshell.Services.Pipewire/PwNodePeakMonitor · https://quickshell.org/docs/types/Quickshell.Io

**PipeWire / WirePlumber**
- filter-chain (PipeWire 1.6.x docs): node types ladspa, lv2, builtin, sofa, ebur128, ffmpeg (+ onnx); builtins incl. mixer, copy, biquads (`bq_lowpass/highpass/bandpass/lowshelf/highshelf/peaking/notch/allpass/raw`, controls Freq/Q/Gain), `param_eq` (loads AutoEq/Squiglink files, up to 8 channels), `convolver`, `delay`, `invert`, `linear`, `dcblock`, `ramp`, `noisegate`, `zeroramp`; SOFA spatializer with live Azimuth/Elevation/Radius; `ebur128` + `lufs2gain`; `capture.volumes` maps sink volume to a graph control. — https://docs.pipewire.org/page_module_filter_chain.html
- Runtime params: `pw-cli s <id> Props '{ params = [ "<key>" <value> ] }'`. — https://docs.pipewire.org/page_man_pipewire-devices_7.html
- Stream processing options: `resample.quality` (default 4), `resample.window`, `channelmix.upmix-method` (psd/none/simple), `channelmix.stereo-widen`, `dither.method` (none/rectangular/triangular/triangular-hf/wannamaker3/shaped5), `dither.noise`. — https://docs.pipewire.org/page_man_pipewire-client_conf_5.html
- `default.clock.allowed-rates` (up to 32 rates; graph switches when devices idle; off by default due to kernel/BT issues). — https://docs.pipewire.org/page_man_pipewire_conf_5.html
- PipeWire 1.6 (Feb 2026): LDAC decoder, 128 channels, audio.layout shorthand, configurable resampler windows, ffmpeg/ONNX in filter-graph; 1.6.6 fixed filter-graph volume restore. — https://phoronix.com/news/PipeWire-1.6 · https://codegraph.jelmer.uk/pipewire/1.6.6-1/NEWS
- Arch `pipewire` 1.6.8 lists libmysofa as a build dependency (SOFA filters compiled in — **VERIFY** the sofa plugin loads); `pipewire-onnx` is a separate package. — https://archlinux.org/packages/extra/x86_64/pipewire/
- WirePlumber smart filters (0.5.x): `filter.smart`, `.name`, `.disabled`, `.targetable`, `.target`, `.before/.after` on the main node; runtime override via `pw-metadata -n filters …`. — https://pipewire.pages.freedesktop.org/wireplumber/policies/smart_filters.html
- Bluetooth: A2DP SBC/SBC-XQ/AAC/aptX/aptX HD/aptX LL/LDAC/FastStream/Opus; LE Audio/LC3 support maturing; `bluez5.a2dp.ldac.quality` auto/hq/sq/mq. — https://wiki.archlinux.org/title/Bluetooth_headset · https://man.archlinux.org/man/extra/pipewire/pipewire-devices.7.en

**Existing Linux DSP (for comparison only)**
- EasyEffects 8.x moved to Qt/QML/Kirigami with a tray icon. — https://ubuntuhandbook.org/index.php/2025/11/easy-effects-8-0-0-switched-from-gtk4-to-qt-kde-framework/
- JamesDSP for Linux 2.7.0: convolver, ViPER-DDC, Liveprog, D-Bus/CLI control; Omarchy issue: its sink captures new streams. — https://github.com/Audio4Linux/JDSP4Linux · https://github.com/Audio4Linux/JDSP4Linux/issues/295
- CamillaDSP v4.x: FIR/IIR engine with native PipeWire backend + websocket control. — https://github.com/henquist/camilladsp
- LSP plugins (LV2) in Arch extra. — https://archlinux.org/packages/extra/x86_64/lsp-plugins-lv2

**Poweramp reference**
- Poweramp: 64-band parametric EQ, per-device presets, bass/treble, DVC, hi-res/USB DAC; beta engine: bit-perfect modes, allowed sample rates, BT codec matching, "limit button overload highlight". — https://powerampapp.com/
- Poweramp Equalizer: 5–32 bands (fixed or custom range), ±15 dB, preamp, presets per device, autosave, limiter + compressor, balance, DVC. — https://forum.powerampapp.com/files/file/71-powerampequalizer-build-890-equ-uniapk/
- Parametric band params (type, channel, gain, freq, Q, colour), AutoEq import. — Poweramp build-911 notes (powerampapp.com)
- AutoEq presets preinstalled, suggested on device connection, presets lockable. — https://www.apkmirror.com/?p=4835644
- DVC: volume controlled inside Poweramp for more EQ headroom; "No DVC headroom gain" used otherwise. — https://forum.powerampapp.com/kb/en_us/faq/using-direct-volume-control-dvc-r42/

**Spatial / Atmos**
- PipeWire ships a HeSuVi 7.1 virtual-surround filter-chain example (HRIR WAV + convolver). — community guide: https://lemmy.nekusoul.de/post/3937/41178
- Cavern decodes E-AC-3 JOC and (with truehdd) TrueHD Atmos; licence heavily discourages commercial use; .NET. — https://github.com/VoidXH/Cavern · https://github.com/truehdd/truehdd
- Steam Audio SDK fully open source (Apache-2.0) since 4.5.2. — https://phoronix.com/news/Steam-Audio-SDK-Fully-Open
- SPARTA/SAF (binauraliser with SOFA + OSC head-tracking; LV2), IEM Plugin Suite (Ambisonics BinauralDecoder; LV2). — https://leomccormack.github.io/Spatial_Audio_Framework/ · https://github.com/tu-studio/IEMPluginSuite
- AutoEq: 4000+ headphone results; ParametricEQ.txt format; preamp required to avoid clipping. — https://github.com/jaakkopasanen/AutoEq (**VERIFY** licence before bundling the database)
