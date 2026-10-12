#!/usr/bin/env python3
"""Generate float32 stereo test WAVs.

usage:
  signals.py sine  out.wav --freq 1000 --db -12 --secs 5
  signals.py multi out.wav --db -24 --secs 6          (log-spaced multitone, 20 Hz - 20 kHz)
  signals.py sweep out.wav --db -12 --secs 6          (exponential sine sweep 20 Hz - 20 kHz)
  signals.py pink  out.wav --db -20 --secs 6
  signals.py imp   out.wav --secs 1                   (single-sample click at 0.25 s, for latency)
  any kind: --only L|R puts the signal on one channel
"""
import argparse

import numpy as np
import soundfile as sf

ap = argparse.ArgumentParser()
ap.add_argument("kind", choices=["sine", "multi", "sweep", "pink", "imp", "silence"])
ap.add_argument("out")
ap.add_argument("--freq", type=float, default=1000.0)
ap.add_argument("--db", type=float, default=-12.0)
ap.add_argument("--secs", type=float, default=5.0)
ap.add_argument("--rate", type=int, default=48000)
ap.add_argument("--lead", type=float, default=0.3, help="leading silence, s")
ap.add_argument("--only", choices=["L", "R"], help="put the signal on one channel; the other is silent")
a = ap.parse_args()

sr = a.rate
n = int(a.secs * sr)
t = np.arange(n) / sr
amp = 10 ** (a.db / 20)
rng = np.random.default_rng(1)

if a.kind == "sine":
    x = amp * np.sin(2 * np.pi * a.freq * t)
elif a.kind == "multi":
    # bin-aligned tones (1 s FFT frames -> 1 Hz bins), log spaced, random phase
    freqs = np.unique(np.round(np.geomspace(20, 20000, 96)))
    x = np.zeros(n)
    for f in freqs:
        x += np.sin(2 * np.pi * f * t + rng.uniform(0, 2 * np.pi))
    x *= amp / np.max(np.abs(x))
elif a.kind == "sweep":
    f1, f2 = 20.0, 20000.0
    k = np.log(f2 / f1)
    x = amp * np.sin(2 * np.pi * f1 * a.secs / k * (np.exp(t / a.secs * k) - 1))
elif a.kind == "pink":
    w = rng.standard_normal(n)
    W = np.fft.rfft(w)
    f = np.fft.rfftfreq(n, 1 / sr)
    f[0] = f[1]
    x = np.fft.irfft(W / np.sqrt(f), n)
    x *= amp / np.sqrt(np.mean(x ** 2))
elif a.kind == "imp":
    x = np.zeros(n)
    x[int(0.25 * sr)] = amp
else:
    x = np.zeros(n)

lead = np.zeros(int(a.lead * sr))
x = np.concatenate([lead, x, lead]).astype(np.float32)
z = np.zeros_like(x)
l, r = {None: (x, x), "L": (x, z), "R": (z, x)}[a.only]
sf.write(a.out, np.stack([l, r], axis=1), sr, subtype="FLOAT")
