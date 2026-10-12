#!/usr/bin/env python3
"""RBJ Audio-EQ-Cookbook biquads: coefficients and combined magnitude response.

usage: rbj.py curve out.json --rate 48000 BAND [BAND ...]
  BAND = type:freq:gain:q   e.g. peak:1000:6:1.414  lowshelf:105:4:0.707  highshelf:8000:-3:0.707
         types: peak lowshelf highshelf lowpass highpass bandpass notch allpass
Writes {"freq": [...], "db": [...]} at the 96 multitone frequencies (and 256 log points with --dense).
"""
import argparse
import json
import math

import numpy as np


def coeffs(kind, f0, gain_db, q, fs):
    """Normalised (b0, b1, b2, a1, a2), a0 = 1."""
    A = 10 ** (gain_db / 40)
    w0 = 2 * math.pi * f0 / fs
    cw, sw = math.cos(w0), math.sin(w0)
    alpha = sw / (2 * q)
    if kind == "peak":
        b = [1 + alpha * A, -2 * cw, 1 - alpha * A]
        a = [1 + alpha / A, -2 * cw, 1 - alpha / A]
    elif kind == "lowshelf":
        sa = 2 * math.sqrt(A) * alpha
        b = [A * ((A + 1) - (A - 1) * cw + sa), 2 * A * ((A - 1) - (A + 1) * cw), A * ((A + 1) - (A - 1) * cw - sa)]
        a = [(A + 1) + (A - 1) * cw + sa, -2 * ((A - 1) + (A + 1) * cw), (A + 1) + (A - 1) * cw - sa]
    elif kind == "highshelf":
        sa = 2 * math.sqrt(A) * alpha
        b = [A * ((A + 1) + (A - 1) * cw + sa), -2 * A * ((A - 1) + (A + 1) * cw), A * ((A + 1) + (A - 1) * cw - sa)]
        a = [(A + 1) - (A - 1) * cw + sa, 2 * ((A - 1) - (A + 1) * cw), (A + 1) - (A - 1) * cw - sa]
    elif kind == "lowpass":
        b = [(1 - cw) / 2, 1 - cw, (1 - cw) / 2]
        a = [1 + alpha, -2 * cw, 1 - alpha]
    elif kind == "highpass":
        b = [(1 + cw) / 2, -(1 + cw), (1 + cw) / 2]
        a = [1 + alpha, -2 * cw, 1 - alpha]
    elif kind == "bandpass":
        b = [alpha, 0.0, -alpha]
        a = [1 + alpha, -2 * cw, 1 - alpha]
    elif kind == "notch":
        b = [1.0, -2 * cw, 1.0]
        a = [1 + alpha, -2 * cw, 1 - alpha]
    elif kind == "allpass":
        b = [1 - alpha, -2 * cw, 1 + alpha]
        a = [1 + alpha, -2 * cw, 1 - alpha]
    else:
        raise ValueError(kind)
    a0 = a[0]
    return (b[0] / a0, b[1] / a0, b[2] / a0, a[1] / a0, a[2] / a0)


def magnitude_db(cs, freqs, fs):
    z = np.exp(-1j * 2 * np.pi * np.asarray(freqs) / fs)
    h = np.ones(len(freqs), dtype=complex)
    for b0, b1, b2, a1, a2 in cs:
        h *= (b0 + b1 * z + b2 * z * z) / (1 + a1 * z + a2 * z * z)
    return 20 * np.log10(np.abs(h))


def parse_band(s):
    k, f, g, q = s.split(":")
    return k, float(f), float(g), float(q)


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("cmd", choices=["curve", "coeffs"])
    ap.add_argument("out")
    ap.add_argument("bands", nargs="+")
    ap.add_argument("--rate", type=float, default=48000)
    ap.add_argument("--dense", action="store_true")
    a = ap.parse_args()
    cs = [coeffs(*parse_band(b), a.rate) for b in a.bands]
    if a.cmd == "coeffs":
        json.dump([dict(zip(["b0", "b1", "b2", "a1", "a2"], c)) for c in cs], open(a.out, "w"), indent=1)
    else:
        fr = np.geomspace(20, 20000, 256) if a.dense else np.unique(np.round(np.geomspace(20, 20000, 96)))
        json.dump({"freq": fr.tolist(), "db": magnitude_db(cs, fr, a.rate).tolist()}, open(a.out, "w"))
