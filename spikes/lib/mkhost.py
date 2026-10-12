#!/usr/bin/env python3
"""Compose a spike host config: base-host.conf + module fragments.

usage: mkhost.py [-D KEY=VALUE ...] out.conf fragment.conf [fragment.conf ...]

Each fragment holds one or more `{ name = libpipewire-module-... }` entries that
are spliced into context.modules. @KEY@ placeholders in fragments are replaced
from -D options.
"""
import argparse
import pathlib

here = pathlib.Path(__file__).resolve().parent

ap = argparse.ArgumentParser()
ap.add_argument("-D", action="append", default=[])
ap.add_argument("out")
ap.add_argument("fragments", nargs="*")
args = ap.parse_args()

defs = dict(d.split("=", 1) for d in args.D)
modules = []
for f in args.fragments:
    text = pathlib.Path(f).read_text()
    for k, v in defs.items():
        text = text.replace(f"@{k}@", v)
    modules.append(text)

base = (here / "base-host.conf").read_text()
base = base.replace("#@MODULES@", "\n".join(modules))
pathlib.Path(args.out).write_text(base)
