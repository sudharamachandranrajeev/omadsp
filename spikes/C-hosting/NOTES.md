# Spike C — hosting: child `pipewire -c` vs in-process `pw_context_load_module` (results)

Run 2026-10-12, PipeWire 1.6.8. `run.sh` reproduces it (`SKIP_C1=1` skips the timing loop). The in-process
host is `inproc.c`: the same `pw_context_load_module()` / `pw_impl_module_destroy()` calls the Rust daemon
would make. Both hosts ran the Spike A LSP chain (para_eq x16 + limiter) as a smart filter on the private sink.

## Rust side (from the crate source, not built)

`pipewire-sys` 0.10.1's `build.rs` generates bindings with `allowlist_function("pw_.*")`, so
`pw_context_load_module` and `pw_impl_module_destroy` exist as raw FFI. `pipewire::context::Context::as_raw_ptr()`
is public in `pipewire` 0.10.1. In-process loading from Rust is a few lines of `unsafe`.

`cargo` couldn't reach crates.io (`cargo search` hung; `curl` to the same hosts answered in ~0.1 s). OpenSnitch is
active, so cargo most likely needs an allow rule. **Phase 1 prerequisite.** The crates were fetched with curl
into the scratchpad only to read them.

## Results

| | child `pipewire -c` | in-process |
|---|---|---|
| Spawn/load → filter output port visible (5 runs) | 49–60 ms | 57–62 ms (incl. exec + connect) |
| `pw_context_load_module` call alone | — | 26–33 ms |
| `pw_impl_module_destroy` | — | 11–51 ms |
| Respawn → port visible | 5–7 ms | — |
| **Reload while playing** (3×, 1 kHz tone) | gap **90–133 ms** + click (−8 dBc) each | gap **85–154 ms** + click (−8 dBc) each |
| RSS with the LSP chain | 37.3 MiB | 37.1 MiB |
| Scheduling of `data-loop.0` | TS (no RT) | TS (no RT) |

A reload is never seamless either way: the filter's nodes disappear, WirePlumber moves the stream to the sink
(unprocessed), then back to the new filter — two relinks.

## Host death (SIGKILL, standing in for a plugin crash)

SIGKILL was used instead of SIGSEGV so systemd-coredump wouldn't record a dump or raise a desktop crash
notification. The effect on audio is the same: the process and its nodes vanish.

- The stream went **straight to the device, unprocessed**, after an **85 ms** gap (`pw-cat → omadsp_spike_out`).
- After respawn it moved back to the filter with a **48 ms** gap (`pw-cat → omadsp_spike_eq`).
- No stuck links or silence. WirePlumber handles both directions on its own.

With in-process hosting the same crash would also kill the daemon: its IPC, its state, and the bar widget's
connection to it.

## Real-time scheduling: none on this machine

Neither host got RT, and neither has PipeWire itself: the daemon's `data-loop.0` is `TS` with no rtprio,
`rtkit-daemon` isn't active, and `ulimit -r` is 0. `module-rt` loads but can't raise priority. 0 xruns at quantum
1024 and 256 so far (Spike A §7), so it's not blocking. Granting RT (rtkit, or a realtime group with rlimits)
would be a system change needing sudo → optional, user's call.

## Verdict

Startup, reload cost and memory are a wash. **Crash isolation decides: host the filter in a separate process
supervised by the daemon.** Two ways to build that child:
1. `pipewire -c <generated conf>`, like Omarchy's speaker tuning — needs a generated config file (runtime dir).
2. `omadspd` re-executing itself in a host mode that calls `pw_context_load_module` through FFI, with the module
   args over a pipe — no config file, one binary, and the daemon controls the module lifecycle directly.

Either way, reloads cost ~0.1 s plus a click. So the chain must have a **fixed topology**, with every stage
bypassable in place: EQ bands `Off`, LSP `enabled`, ramped playback `volume`. Normal use then never reloads.
