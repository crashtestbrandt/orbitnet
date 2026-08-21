#!/usr/bin/env python3
"""Build the Asset Library zip, and prove it carries the libraries the descriptor names.

PYTHON'S zipfile RATHER THAN THE `zip` BINARY. The publish runner has neither `zip` nor `unzip`: a
release dry run failed here with "zip: command not found" after all three cross-platform builds had
already succeeded. python3 is present on every runner this repository uses and needs no package
install, so building the archive and reading it back both go through the standard library.

Usage: tools/make-assetlib-zip.py <version> [outdir]
"""
import os, sys, zipfile

version = sys.argv[1] if len(sys.argv) > 1 else sys.exit("usage: make-assetlib-zip.py <version> [outdir]")
outdir = sys.argv[2] if len(sys.argv) > 2 else "build"
BIN = os.path.join("addons", "orbitnet_native", "bin")

# The manifest is written AFTER this archive, because it hashes the archive -- so the copy sitting in
# the tree is the PREVIOUS tag's, and shipping it would hand a user digests matching none of the
# libraries beside it.
SKIP = {os.path.join("addons", "orbitnet_native", "binaries.json")}

os.makedirs(outdir, exist_ok=True)
out = os.path.join(outdir, "orbitnet-%s.zip" % version)

libs = 0
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
    for root in ("addons/orbitnet", "addons/orbitnet_native"):
        if not os.path.isdir(root):
            sys.exit("::error::%s is missing; nothing to package" % root)
        for dirpath, _dirnames, filenames in os.walk(root):
            for name in sorted(filenames):
                full = os.path.join(dirpath, name)
                if full in SKIP:
                    continue
                z.write(full, full)
                if os.path.normpath(dirpath) == BIN:
                    libs += 1

# bin/ is gitignored, so a clean checkout would otherwise package an addon with no backend at all and
# say nothing. Count what went in rather than trusting the walk.
expected = len(os.listdir(BIN)) if os.path.isdir(BIN) else 0
if expected == 0:
    sys.exit("::error::%s holds no libraries; the zip would ship a facade over nothing" % BIN)
if libs != expected:
    sys.exit("::error::the zip carries %d libraries; %s holds %d" % (libs, BIN, expected))

with zipfile.ZipFile(out) as z:
    entries = len(z.namelist())
print("wrote %s: %d bytes, %d entries, %d libraries" % (out, os.path.getsize(out), entries, libs))
