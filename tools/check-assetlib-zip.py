#!/usr/bin/env python3
"""Assemble the Asset Library zip, assert what it carries, unpack it into a throwaway project and boot it.

Every other gate in this repository loads a library it just built, out of the working tree. The zip a
consumer installs is assembled by a path none of them touches -- `tools/make-assetlib-zip.py`, run from a
tree whose `addons/orbitnet_native/bin/` was filled by downloaded build artifacts, with the licence files
copied in beside the addon. A zip that is missing a library, carries a Git LFS pointer where a library
should be, or unpacks to a layout Godot does not resolve fails for the user and for nobody else.

This is the gate for that artifact. It has two halves and both are kept.

Contents, read out of the archive itself
----------------------------------------
- Both addon directories are present, and nothing outside them is.
- `net.gd` and the **EditorPlugin script `plugin.cfg` names** are both present and non-empty. The release
  body tells a user to enable the plugin from the editor's plugin list, and that is the file the editor
  loads to do it.
- Every file under `bin/` is one the zip's own descriptor names, is non-empty, and does not begin with Git
  LFS pointer text. With `--complete`, the set under `bin/` must equal the descriptor's named set exactly.
- The licence files ride inside the payload. An addon installed from a zip carries no repository with it.
  Which files those are is read out of `release.yml`'s own `cp ... addons/orbitnet/` line rather than
  re-spelled here, so a change to what the release copies in is a change to what this asserts.
- `binaries.json` stays out. It hashes the archive, so the copy in the tree is the previous tag's and
  shipping it would hand a user digests matching none of the libraries beside it.
- No synced addon copy nested inside either root, no `.git` or `.godot` directory, no member path that is
  absolute or climbs out of the extraction directory.

`tools/make-assetlib-zip.py` already counts the libraries it packaged against what `bin/` held, so it
catches a walk that dropped one. It cannot speak for the licences, for strays, for pointer text, or for a
`bin/` that was already short before packaging started. Those are what this adds.

Boot, from the unpacked zip
---------------------------
The archive is unpacked, a throwaway `project.godot` is written beside it declaring the `Net` autoload and
the enabled plugin -- which is what enabling the plugin in the editor writes -- and two existing scripts
run against that tree:

- `tools/lint-gdscript.sh` loads the project, so every `.gd` file in the zip's GDScript half compiles and
  the `Net` autoload instantiates against the zip's own binaries.
- `tools/orbitnet-smoke.sh --skip-build` asserts the backend half: magic bytes, the entry symbol, the
  classes registering, exported properties binding, signals reaching GDScript, ticks advancing. `--skip-build`
  is the whole point of pointing it here -- the zip supplies the binaries and nothing is compiled twice.

Both are copied into the unpacked tree and run with it as their root, so they read the zip's files and
never the repository's.

The enabled-plugin line mirrors what the editor writes when a user ticks the plugin. Neither pass fails on
an addon script that did not make it into the zip: the load pass logs `Attempt to open script ... 'File not
found'` and exits 0, and that text matches none of `lint-gdscript.sh`'s error patterns. The plugin's own
script is therefore asserted by the contents half above rather than here.

Negative control
----------------
`--self-test` synthesizes a well-formed archive from the real descriptor, asserts it passes, then mutates
it once per failure class and asserts each mutation is caught. One case per branch of the contents half:

- a library the descriptor names removed, one replaced by Git LFS pointer text, one that is empty, one the
  descriptor does not name,
- the descriptor missing, the descriptor naming no `bin/` paths,
- `plugin.cfg` missing, the script it names missing, `net.gd` missing,
- a licence dropped, a licence that is empty,
- a stray file outside both addon roots, a `.godot` directory packaged, `binaries.json` included, a nested
  addon copy, a member path that climbs out of the extraction directory.

It builds no library, starts no Godot and reads no artifact, so it runs in milliseconds next to the other
standard-library gates. Add a case here with every branch added to the contents half -- a branch with no
case is gated by nothing.

Where this runs, and why in three places
----------------------------------------
- `just assetlib-check` is the local end-to-end run: assemble from this tree, assert, boot.
- `check.yml` runs `--self-test` in the cheap gates job, and the full run in the Godot job after the load
  smoke. A pull request cannot produce the real artifact -- only one platform's libraries exist there -- but
  it is where a change to the packaging script, the descriptor or the addon layout lands, and that is the
  half a pull request can actually break.
- `release.yml` runs it with `--complete` on the zip it just built, before that zip is published. That is
  the only place the real artifact exists: every platform's libraries, downloaded rather than built locally,
  with the licences copied in. A failure there fails the release rather than annotating it.

Python rather than shell, for the same reason `tools/make-assetlib-zip.py` is. The publish runner has
neither `zip` nor `unzip`, and a release dry run failed on exactly that after every cross-platform build had
already succeeded. `zipfile` needs no package install on any runner this repository uses.

Usage:
  tools/check-assetlib-zip.py [--zip PATH] [--complete] [--skip-boot] [--self-test]

  --zip PATH    check an archive that already exists instead of assembling one
  --complete    require every library the descriptor names, not just the ones this host can build
  --skip-boot   contents only, no Godot
  --self-test   run the negative control and exit

Env: GODOT (binary or wrapper handed to the two scripts; they default to the unpacked tools/godot-quiet.sh)
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
import zipfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

ADDON_ROOTS = ("addons/orbitnet/", "addons/orbitnet_native/")
BIN_PREFIX = "addons/orbitnet_native/bin/"
DESCRIPTOR = "addons/orbitnet_native/orbitnet.gdextension"
MANIFEST = "addons/orbitnet_native/binaries.json"
PLUGIN_CFG = "addons/orbitnet/plugin.cfg"
FACADE = "addons/orbitnet/net.gd"
RELEASE_WF = ".github/workflows/release.yml"

# A file whose presence means the packaging walk picked up something that is not addon source.
FORBIDDEN_SEGMENTS = {".git", ".godot", "__pycache__", "node_modules", ".DS_Store"}

LFS_MARK = b"git-lfs.github.com"


# --------------------------------------------------------------------------------------------------
# what the archive is measured against


def descriptor_names(text):
    """The library basenames a .gdextension resolves to. Several entries may name one file."""
    return sorted(set(os.path.basename(p) for p in re.findall(r"res://addons/orbitnet_native/bin/([^\"\s]+)", text)))


def plugin_script(text):
    """The EditorPlugin script `plugin.cfg` names, relative to the plugin.cfg's own directory."""
    found = re.search(r'^\s*script\s*=\s*"([^"]+)"', text, re.M)
    return found.group(1) if found else ""


def release_licences(path):
    """The files release.yml copies into addons/orbitnet/ before packaging.

    Read out of the workflow rather than listed here, so the two cannot drift. A shape change in that step
    is reported as one, the way tools/check-descriptor-parity.sh reports a descriptor it can no longer
    parse -- silence would be indistinguishable from a release that stopped shipping its licences.
    """
    with open(path, "r", encoding="utf-8") as fh:
        body = fh.read()
    found = re.search(r"^\s*cp ([^\n]+?) addons/orbitnet/\s*$", body, re.M)
    if not found:
        sys.exit(
            "::error::found no `cp <files> addons/orbitnet/` line in %s -- the packaging step's shape "
            "changed and this check needs updating." % path
        )
    names = found.group(1).split()
    if not names:
        sys.exit("::error::%s copies no files into addons/orbitnet/ before packaging." % path)
    return names


# --------------------------------------------------------------------------------------------------
# the contents assertions


def inspect(zip_path, licences, complete):
    """Every problem the archive has, as a list of sentences. An empty list is a pass."""
    problems = []
    with zipfile.ZipFile(zip_path) as z:
        infos = [i for i in z.infolist() if not i.filename.endswith("/")]
        names = set(i.filename for i in infos)

        for info in infos:
            name = info.filename
            parts = name.split("/")
            if name.startswith("/") or ".." in parts or (len(name) > 1 and name[1] == ":"):
                problems.append("member path %r is absolute or climbs out of the extraction directory" % name)
                continue
            if not name.startswith(ADDON_ROOTS):
                problems.append("%s is outside both addon directories" % name)
                continue
            bad = FORBIDDEN_SEGMENTS.intersection(parts)
            if bad:
                problems.append("%s carries a %s path segment" % (name, sorted(bad)[0]))
            # The canonical addon directories are the two roots. A second `addons/` below one of them is a
            # synced mirror copy that was packaged by mistake.
            if "addons" in parts[1:]:
                problems.append("%s is a nested addon copy, not canonical addon source" % name)

        if MANIFEST in names:
            problems.append(
                "%s is in the zip. It hashes this archive, so the copy in the tree is the previous "
                "tag's and its digests match none of the libraries beside it." % MANIFEST
            )

        for required in (DESCRIPTOR, PLUGIN_CFG, FACADE):
            if required not in names:
                problems.append("%s is missing; the addon cannot install without it" % required)

        for licence in licences:
            member = "addons/orbitnet/" + licence
            if member not in names:
                problems.append(
                    "%s is missing. An addon installed from a zip carries no repository with it." % member
                )
            elif z.getinfo(member).file_size == 0:
                problems.append("%s is empty" % member)

        # The plugin half of the install. `plugin.cfg` present is not enough: the editor loads the script
        # it names, and a zip missing that file installs and then fails at the moment the user ticks the
        # plugin. Read the name out of the zip's own plugin.cfg rather than spelling it here.
        if PLUGIN_CFG in names:
            script = plugin_script(z.read(PLUGIN_CFG).decode("utf-8"))
            if not script:
                problems.append("%s names no script=; the plugin cannot be enabled" % PLUGIN_CFG)
            else:
                member = "addons/orbitnet/" + script
                if member not in names:
                    problems.append(
                        "%s is the EditorPlugin script %s names and is missing from the zip; enabling the "
                        "plugin fails for the user" % (member, PLUGIN_CFG)
                    )
                elif z.getinfo(member).file_size == 0:
                    problems.append("%s is empty" % member)

        named = descriptor_names(z.read(DESCRIPTOR).decode("utf-8")) if DESCRIPTOR in names else []
        if DESCRIPTOR in names and not named:
            problems.append("%s names no bin/ library paths; the descriptor shape changed" % DESCRIPTOR)

        present = sorted(n[len(BIN_PREFIX):] for n in names if n.startswith(BIN_PREFIX))
        if not present:
            problems.append("%s holds no libraries; the zip ships a facade over nothing" % BIN_PREFIX)
        for lib in present:
            member = BIN_PREFIX + lib
            if lib not in named:
                problems.append("%s is a library the descriptor never loads" % member)
            if z.getinfo(member).file_size == 0:
                problems.append("%s is empty" % member)
                continue
            with z.open(member) as fh:
                head = fh.read(512)
            if LFS_MARK in head:
                problems.append(
                    "%s is a Git LFS pointer, not a library. Every platform's loader rejects it and "
                    "none of them says why." % member
                )
        if complete:
            for lib in named:
                if lib not in present:
                    problems.append("%s%s is named by the descriptor and missing from the zip" % (BIN_PREFIX, lib))

    return problems


# --------------------------------------------------------------------------------------------------
# assembling one to check


def run_stage(stage, argv, **kw):
    """Run a sub-process, reporting a non-zero exit as this script's own ::error:: line.

    Every contents failure exits with one, and CI annotates on it. A CalledProcessError traceback wrapped
    around the sub-process's own message is neither an annotation nor readable locally -- the likeliest
    failure of all is an empty addons/orbitnet_native/bin/, where make-assetlib-zip.py already says so.
    """
    try:
        subprocess.run(argv, check=True, **kw)
    except subprocess.CalledProcessError as exc:
        sys.exit("::error::%s failed (exit %d): %s" % (stage, exc.returncode, " ".join(argv)))


def assemble(outdir, licences):
    """Package the working tree the way release.yml does, in a staging copy.

    A staging copy rather than the working tree, because the release step copies four licence files into
    `addons/orbitnet/` and deletes them again -- a run interrupted part way through would leave them behind,
    where `just addon-drift` would then report them as an edited addon.
    """
    stage = os.path.join(outdir, "stage")
    os.makedirs(stage)
    for root in ADDON_ROOTS:
        src = os.path.join(ROOT, root.rstrip("/"))
        if not os.path.isdir(src):
            sys.exit("::error::%s is missing from this checkout; nothing to package" % root)
        shutil.copytree(src, os.path.join(stage, root.rstrip("/")))
    for licence in licences:
        shutil.copy2(os.path.join(ROOT, licence), os.path.join(stage, "addons", "orbitnet", licence))

    version = "dev"
    cfg = os.path.join(stage, PLUGIN_CFG)
    with open(cfg, "r", encoding="utf-8") as fh:
        found = re.search(r'^version="([^"]+)"', fh.read(), re.M)
    if found:
        version = found.group(1)

    run_stage(
        "packaging the zip",
        [sys.executable, os.path.join(ROOT, "tools", "make-assetlib-zip.py"), version, "build"],
        cwd=stage,
    )
    return os.path.join(stage, "build", "orbitnet-%s.zip" % version)


# --------------------------------------------------------------------------------------------------
# booting the unpacked archive

PROJECT_GODOT = """\
config_version=5

[application]

config/name="orbitnet-assetlib-install"
run/main_scene="res://main.tscn"
config/features=PackedStringArray("4.4")

[autoload]

Net="*res://addons/orbitnet/net.gd"

[editor_plugins]

enabled=PackedStringArray("res://addons/orbitnet/plugin.cfg")
"""

MAIN_TSCN = """\
[gd_scene format=3]

[node name="Main" type="Node"]
"""

# The scripts the unpacked tree needs to be able to run itself. orbitnet-smoke.sh and lint-gdscript.sh both
# resolve their root as the parent of their own directory, so a copy placed here reads the zip's files.
STAGED_TOOLS = ("orbitnet-smoke.sh", "lint-gdscript.sh", "build-native.sh", "godot-quiet.sh")


def boot(zip_path, workdir):
    install = os.path.join(workdir, "install")
    with zipfile.ZipFile(zip_path) as z:
        z.extractall(install)

    tools = os.path.join(install, "tools")
    os.makedirs(tools, exist_ok=True)
    for name in STAGED_TOOLS:
        dst = os.path.join(tools, name)
        shutil.copy2(os.path.join(ROOT, "tools", name), dst)
        os.chmod(dst, 0o755)

    with open(os.path.join(install, "project.godot"), "w", encoding="utf-8") as fh:
        fh.write(PROJECT_GODOT)
    with open(os.path.join(install, "main.tscn"), "w", encoding="utf-8") as fh:
        fh.write(MAIN_TSCN)

    # GODOT and GODOT_BIN pass straight through. Unset, both scripts fall back to the copy of
    # godot-quiet.sh staged above, which resolves `godot` on PATH.
    env = dict(os.environ)

    print("\n-- the zip's GDScript half loads, with Net live against the zip's own binaries")
    run_stage("the GDScript load pass", [os.path.join(tools, "lint-gdscript.sh"), "."], cwd=install, env=env)

    print("\n-- the zip's binaries register their classes (no rebuild)")
    run_stage(
        "the extension smoke", [os.path.join(tools, "orbitnet-smoke.sh"), "--skip-build"], cwd=install, env=env
    )


# --------------------------------------------------------------------------------------------------
# the negative control


def _write_zip(path, entries):
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        for name, data in sorted(entries.items()):
            z.writestr(name, data)


def self_test(licences):
    with open(os.path.join(ROOT, DESCRIPTOR), "r", encoding="utf-8") as fh:
        descriptor = fh.read()
    named = descriptor_names(descriptor)
    if not named:
        sys.exit("::error::%s names no libraries; the self-test has nothing to build against" % DESCRIPTOR)

    # A plausible ELF header and some body, so the baseline is rejected for nothing but a mutation below.
    library = b"\x7fELF" + b"\x02\x01\x01" + bytes(509)
    cfg = '[plugin]\n\nname="OrbitNet"\nversion="0.0.0"\nscript="orbitnet.gd"\n'
    # Read back out of the fixture's own plugin.cfg, so the case that deletes this file and the assertion
    # that misses it are reading the same name.
    plugin_member = "addons/orbitnet/" + plugin_script(cfg)
    base = {
        DESCRIPTOR: descriptor,
        DESCRIPTOR + ".uid": "uid://selftest",
        PLUGIN_CFG: cfg,
        plugin_member: "@tool\nextends EditorPlugin\n",
        FACADE: "extends Node\n",
        "addons/orbitnet/README.md": "# OrbitNet\n",
    }
    for licence in licences:
        base["addons/orbitnet/" + licence] = "a licence\n"
    for lib in named:
        base[BIN_PREFIX + lib] = library

    cases = []

    dropped = dict(base)
    del dropped[BIN_PREFIX + named[0]]
    cases.append(("a library the descriptor names is missing", dropped, True))

    pointer = dict(base)
    pointer[BIN_PREFIX + named[0]] = (
        "version https://git-lfs.github.com/spec/v1\noid sha256:%s\nsize 9652864\n" % ("0" * 64)
    )
    cases.append(("a library is a Git LFS pointer", pointer, False))

    empty = dict(base)
    empty[BIN_PREFIX + named[0]] = b""
    cases.append(("a library is empty", empty, False))

    no_licence = dict(base)
    del no_licence["addons/orbitnet/" + licences[0]]
    cases.append(("a licence file is missing", no_licence, False))

    stray = dict(base)
    stray["README.md"] = "# not addon source\n"
    cases.append(("a file outside both addon directories", stray, False))

    with_manifest = dict(base)
    with_manifest[MANIFEST] = '{"tag": "v0.0.0", "assets": {}}\n'
    cases.append(("binaries.json rode along", with_manifest, False))

    nested = dict(base)
    nested["addons/orbitnet/addons/orbitnet/net.gd"] = "extends Node\n"
    cases.append(("a synced addon copy nested inside the payload", nested, False))

    unnamed = dict(base)
    unnamed[BIN_PREFIX + "liborbitnet.aix.template_debug.ppc64.so"] = library
    cases.append(("a library the descriptor never loads", unnamed, False))

    no_facade = dict(base)
    del no_facade[FACADE]
    cases.append(("net.gd is missing", no_facade, False))

    no_descriptor = dict(base)
    del no_descriptor[DESCRIPTOR]
    cases.append(("the descriptor is missing", no_descriptor, False))

    no_cfg = dict(base)
    del no_cfg[PLUGIN_CFG]
    cases.append(("plugin.cfg is missing", no_cfg, False))

    no_plugin_script = dict(base)
    del no_plugin_script[plugin_member]
    cases.append(("the EditorPlugin script plugin.cfg names is missing", no_plugin_script, False))

    shapeless = dict(base)
    shapeless[DESCRIPTOR] = "[configuration]\nentry_symbol = \"orbitnet_init\"\n"
    cases.append(("the descriptor names no bin/ libraries", shapeless, False))

    empty_licence = dict(base)
    empty_licence["addons/orbitnet/" + licences[0]] = ""
    cases.append(("a licence file is empty", empty_licence, False))

    packaged_godot = dict(base)
    packaged_godot["addons/orbitnet/.godot/uid_cache.bin"] = b"\x00"
    cases.append(("a .godot directory rode along", packaged_godot, False))

    traversal = dict(base)
    traversal["../escaped.gd"] = "extends Node\n"
    cases.append(("a member path that climbs out of the extraction directory", traversal, False))

    work = tempfile.mkdtemp(prefix="orbitnet-assetlib-selftest-")
    failures = []
    try:
        good = os.path.join(work, "baseline.zip")
        _write_zip(good, base)
        problems = inspect(good, licences, complete=True)
        if problems:
            failures.append("the well-formed baseline was rejected: %s" % "; ".join(problems))
        else:
            print("  baseline (%d libraries, %d licences) passes" % (len(named), len(licences)))

        for label, entries, needs_complete in cases:
            path = os.path.join(work, "case.zip")
            _write_zip(path, entries)
            problems = inspect(path, licences, complete=needs_complete)
            if not problems:
                failures.append("went uncaught: %s" % label)
            else:
                print("  caught: %s -- %s" % (label, problems[0]))
    finally:
        shutil.rmtree(work, ignore_errors=True)

    if failures:
        for line in failures:
            print("::error::assetlib self-test: %s" % line, file=sys.stderr)
        return 1
    print("assetlib self-test passed: %d failure classes, each caught." % len(cases))
    return 0


# --------------------------------------------------------------------------------------------------


def main():
    # Line buffering, so this script's own lines and the Godot output of the scripts it runs interleave in
    # the order they happened. Block buffering puts every line below after the whole boot, which in a CI
    # log reads as the wrong file having been checked.
    sys.stdout.reconfigure(line_buffering=True)

    ap = argparse.ArgumentParser(add_help=True)
    ap.add_argument("--zip", dest="zip_path", help="check an archive that already exists")
    ap.add_argument("--complete", action="store_true", help="require every library the descriptor names")
    ap.add_argument("--skip-boot", action="store_true", help="contents only, no Godot")
    ap.add_argument("--self-test", action="store_true", help="run the negative control and exit")
    args = ap.parse_args()

    licences = release_licences(os.path.join(ROOT, RELEASE_WF))

    if args.self_test:
        return self_test(licences)

    work = tempfile.mkdtemp(prefix="orbitnet-assetlib-")
    try:
        zip_path = args.zip_path
        if zip_path:
            zip_path = os.path.abspath(zip_path)
            if not os.path.isfile(zip_path):
                sys.exit("::error::no archive at %s" % zip_path)
        else:
            zip_path = assemble(work, licences)

        print("\nchecking %s (%d bytes)" % (zip_path, os.path.getsize(zip_path)))
        problems = inspect(zip_path, licences, complete=args.complete)
        if problems:
            print("::error::the Asset Library zip is not installable:", file=sys.stderr)
            for line in problems:
                print("  %s" % line, file=sys.stderr)
            return 1
        with zipfile.ZipFile(zip_path) as z:
            entries = len([i for i in z.infolist() if not i.filename.endswith("/")])
            libs = len([n for n in z.namelist() if n.startswith(BIN_PREFIX)])
        print(
            "contents pass: %d entries, %d libraries%s, %d licences, nothing outside the two addon directories"
            % (entries, libs, " (every one the descriptor names)" if args.complete else "", len(licences))
        )

        if args.skip_boot:
            print("--skip-boot: the installed project was not booted.")
            return 0
        boot(zip_path, work)
    finally:
        shutil.rmtree(work, ignore_errors=True)

    print("\nassetlib zip check passed: the archive installs into a clean project and the extension loads.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
