#!/usr/bin/env python3
"""Measurement helpers for the omadsp spikes. All levels in dBFS (1.0 = 0 dBFS).

usage:
  measure.py level    rec.wav --freq 1000 [--freq 3000 ...] [--start S --len L]
  measure.py rms      rec.wav
  measure.py null     a.wav b.wav
  measure.py response ref.wav rec.wav [--json out.json]
  measure.py latency  ref.wav rec.wav
  measure.py xlat     rec.wav            (lag of channel 2 behind channel 1, one recorder)
  measure.py gaps     rec.wav [--min-ms 1]
  measure.py clicks   rec.wav --freq 1000
"""
import argparse
import json
import sys

import numpy as np
import soundfile as sf
from scipy import signal


def db(x):
    return 20 * np.log10(np.maximum(np.abs(x), 1e-12))


def load(path):
    x, sr = sf.read(path, dtype="float64", always_2d=True)
    return x, sr


def active(x, thresh=1e-4):
    """Indices [a, b) spanning the non-silent part of the mono sum."""
    m = np.max(np.abs(x), axis=1)
    idx = np.nonzero(m > thresh)[0]
    if len(idx) == 0:
        return 0, 0
    return idx[0], idx[-1] + 1


def tone_level(seg, sr, f):
    """Amplitude (dBFS peak) of a sine at f, flat-top windowed FFT."""
    n = len(seg)
    w = signal.windows.flattop(n)
    X = np.fft.rfft(seg * w)
    k = int(round(f * n / sr))
    lo, hi = max(k - 3, 0), min(k + 4, len(X))
    amp = np.max(np.abs(X[lo:hi])) * 2 / np.sum(w)
    return db(amp)


def cmd_level(a):
    x, sr = load(a.wav)
    s, e = active(x)
    if a.start is not None:
        s = int(a.start * sr)
    ln = int((a.len or 1.0) * sr)
    # steady part: skip 0.3 s after onset
    s = s + int(0.3 * sr) if a.start is None else s
    seg = x[s : s + ln]
    out = {}
    for f in a.freq:
        out[f] = [round(float(tone_level(seg[:, c], sr, f)), 3) for c in range(x.shape[1])]
    print(json.dumps(out))


def cmd_rms(a):
    x, sr = load(a.wav)
    s, e = active(x)
    seg = x[s:e] if e > s else x
    print(json.dumps({
        "active_s": round((e - s) / sr, 3),
        "rms_db": [round(float(db(np.sqrt(np.mean(seg[:, c] ** 2)))), 3) for c in range(x.shape[1])],
        "peak_db": [round(float(db(np.max(np.abs(seg[:, c])))), 3) for c in range(x.shape[1])],
    }))


def align(a, b, maxlag=2048):
    """Lag (samples, >= 0) to drop from the start of b so it lines up with a.
    Both are already trimmed to their active span, so the true lag is small."""
    n = min(len(a), len(b), 2 * 48000)
    am = a[:n, 0]
    bm = b[: n + maxlag, 0]
    c = signal.correlate(bm, am, mode="full", method="fft")
    zero = len(am) - 1
    lo, hi = zero - maxlag, zero + maxlag + 1
    lo = max(lo, 0)
    return max(int(np.argmax(np.abs(c[lo:hi]))) + lo - zero, 0)


def cmd_null(a):
    x, sr = load(a.a)
    y, _ = load(a.b)
    sx, ex = active(x)
    sy, ey = active(y)
    x = x[sx:ex]
    y = y[sy:ey]
    lag = align(x, y)
    y = y[lag:]
    n = min(len(x), len(y))
    # drop the edges, which see the stream start/stop
    edge = int(0.2 * sr)
    d = x[edge : n - edge] - y[edge : n - edge]
    print(json.dumps({
        "lag": lag,
        "compared_s": round(len(d) / sr, 3),
        "signal_peak_db": round(float(db(np.max(np.abs(x[edge : n - edge])))), 2),
        "diff_peak_db": round(float(db(np.max(np.abs(d)))), 2),
        "diff_rms_db": round(float(db(np.sqrt(np.mean(d ** 2)))), 2),
        "bit_identical": bool(np.max(np.abs(d)) == 0.0),
    }))


def multitone_freqs():
    return np.unique(np.round(np.geomspace(20, 20000, 96)))


def cmd_response(a):
    x, sr = load(a.ref)
    y, _ = load(a.rec)
    sx, _ = active(x)
    sy, _ = active(y)
    # steady 4 s starting 1 s after onset, 1 s frames averaged
    def spec(z, s):
        frames = [z[s + sr * (1 + i) : s + sr * (2 + i), 0] for i in range(4)]
        return np.mean([np.fft.rfft(f) for f in frames], axis=0)
    X = spec(x, sx)
    Y = spec(y, sy)
    fr = multitone_freqs()
    k = fr.astype(int)
    H = db(np.abs(Y[k]) / np.abs(X[k]))
    res = {"freq": fr.tolist(), "db": [round(float(v), 3) for v in H]}
    if a.json:
        json.dump(res, open(a.json, "w"))
    if a.expect:
        exp = json.load(open(a.expect))
        e = np.interp(np.log(fr), np.log(exp["freq"]), exp["db"])
        err = H - e
        res["max_abs_err_db"] = round(float(np.max(np.abs(err))), 3)
        res["worst_freq"] = float(fr[int(np.argmax(np.abs(err)))])
    print(json.dumps({k2: v for k2, v in res.items() if k2 not in ("freq", "db")} if a.quiet else res))


def cmd_latency(a):
    x, sr = load(a.ref)
    y, _ = load(a.rec)
    c = signal.correlate(y[:, 0], x[:, 0], mode="full", method="fft")
    lag = int(np.argmax(np.abs(c))) - (len(x) - 1)
    print(json.dumps({"lag_samples": lag, "lag_ms": round(lag / sr * 1000, 3)}))


def cmd_xlat(a):
    """Both channels come from one recorder (FL = before, FR = after), so they share a clock
    and a start time and the cross-correlation peak is the processing latency."""
    x, sr = load(a.wav)
    s, e = active(x)
    u, v = x[s:e, 0], x[s:e, 1]
    c = signal.correlate(v, u, mode="full", method="fft")
    lag = int(np.argmax(np.abs(c))) - (len(u) - 1)
    print(json.dumps({"lag_samples": lag, "lag_ms": round(lag / sr * 1000, 3),
                      "gain_db": round(float(db(np.max(np.abs(c)) / np.sum(u * u))), 2)}))


def cmd_gaps(a):
    x, sr = load(a.wav)
    s, e = active(x)
    m = np.max(np.abs(x[s:e]), axis=1) < 1e-7
    # run lengths of silence inside the active span
    gaps = []
    run = 0
    for i, z in enumerate(m):
        if z:
            run += 1
        else:
            if run >= a.min_ms * sr / 1000:
                gaps.append((round((s + i - run) / sr, 4), round(run / sr * 1000, 2)))
            run = 0
    print(json.dumps({"active_s": round((e - s) / sr, 3), "gaps": gaps[:50], "count": len(gaps)}))


def cmd_clicks(a):
    """Remove the tone with a narrow notch, then look at the residual's peak in 5 ms windows."""
    x, sr = load(a.wav)
    s, e = active(x)
    z = x[s + int(0.2 * sr) : e - int(0.2 * sr), 0]
    b, aa = signal.iirnotch(a.freq, 2.0, sr)
    r = signal.filtfilt(b, aa, z)
    # high-pass the residual too: level changes of the tone itself are slow
    hb, ha = signal.butter(4, 4000, "high", fs=sr)
    r = signal.filtfilt(hb, ha, r)
    win = int(0.005 * sr)
    nwin = len(r) // win
    pk = np.max(np.abs(r[: nwin * win].reshape(nwin, win)), axis=1)
    tone = np.sqrt(2) * np.sqrt(np.mean(z ** 2))
    worst = int(np.argmax(pk))
    print(json.dumps({
        "tone_peak_db": round(float(db(tone)), 2),
        "residual_peak_db": round(float(db(pk[worst])), 2),
        "residual_median_db": round(float(db(np.median(pk))), 2),
        "worst_at_s": round(worst * win / sr, 3),
        "windows_over_-60dBc": int(np.sum(db(pk) - db(tone) > -60)),
    }))


ap = argparse.ArgumentParser()
sp = ap.add_subparsers(dest="cmd", required=True)
p = sp.add_parser("level"); p.add_argument("wav"); p.add_argument("--freq", type=float, action="append", required=True)
p.add_argument("--start", type=float); p.add_argument("--len", type=float)
p = sp.add_parser("rms"); p.add_argument("wav")
p = sp.add_parser("null"); p.add_argument("a"); p.add_argument("b")
p = sp.add_parser("response"); p.add_argument("ref"); p.add_argument("rec"); p.add_argument("--json"); p.add_argument("--expect"); p.add_argument("--quiet", action="store_true")
p = sp.add_parser("latency"); p.add_argument("ref"); p.add_argument("rec")
p = sp.add_parser("xlat"); p.add_argument("wav")
p = sp.add_parser("gaps"); p.add_argument("wav"); p.add_argument("--min-ms", type=float, default=1.0)
p = sp.add_parser("clicks"); p.add_argument("wav"); p.add_argument("--freq", type=float, required=True)
a = ap.parse_args()
globals()["cmd_" + a.cmd](a)
