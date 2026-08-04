#!/usr/bin/env bash
# godot-quiet.sh -- run Godot, dropping ONE known-benign upstream diagnostic on stderr while leaving
# stdout (and its tty), every other stderr line, and Godot's exit code untouched.
#
# A vendored GodotSteam-Server GDExtension embeds class-reference docs containing a <constant> with no
# `value=` attribute, so Godot's doc loader logs this once at editor/tool startup:
#     ERROR: Condition "!parser->has_attribute("value")" is true. Returning: ERR_FILE_CORRUPT
#        at: _load (editor/doc/doc_tools.cpp:NNNN)
# It is cosmetic -- it fails no gate (tools/lint-gdscript.sh matches only real GDScript errors, and every
# probe reports its own result and exit code) -- but it is baked into a precompiled .so nobody here can
# edit, so exactly those two lines are dropped rather than muting all of stderr. This repo does not vendor
# GodotSteam at all, so on a normal checkout the wrapper is a harmless pass-through; it exists so that a
# developer who HAS installed GodotSteam to exercise the Steam transport gets a clean log too.
# Override the binary with GODOT_BIN (default: `godot` on PATH).
set -u

# The one diagnostic is two lines: the ERROR condition line, and its `at: _load (... doc_tools.cpp:NNNN)`
# follow-up. Matched version-agnostically (no line number pinned) and specifically enough not to swallow
# any other doc/parse error.
pattern='!parser->has_attribute\("value"\).*ERR_FILE_CORRUPT|_load \(editor/doc/doc_tools\.cpp'

# Filter stderr ONLY (via process substitution) so stdout keeps its tty -- interactive `just run` stays
# live and unbuffered. $? after the redirection is Godot's exit status, not grep's; `wait` lets the
# filter drain before we exit.
"${GODOT_BIN:-godot}" "$@" 2> >(grep --line-buffered -vE "$pattern" >&2)
status=$?
wait 2>/dev/null || true
exit "$status"
