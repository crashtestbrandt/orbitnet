#!/usr/bin/env bash
# Write <project>/.godot/extension_list.cfg for a project that has never been imported.
#
# Godot 4.7.2 loads an extension it finds during its first scan mid-session, and a headless session that
# did so crashes at exit: signal 11 on macOS, 0xC0000005 on Windows (godotengine/godot#123511,
# godot-rust/gdext#1711). With the list written first, Godot loads the extensions at startup and the
# import exits 0.
#
# The list is every *.gdextension Godot's scan would find, as res:// paths: hidden directories and any
# directory holding a .gdignore are skipped, as Godot skips them. An existing list is Godot's own and is
# left alone. A project with no extension gets no file.
#
# Usage: tools/seed-extension-list.sh <project_dir>
set -euo pipefail

dir="${1:?usage: $0 <project_dir>}"
list="$dir/.godot/extension_list.cfg"
[ -e "$list" ] && exit 0

paths="$(cd "$dir" && find . -mindepth 1 \
	\( -name '.*' -o \( -type d -exec test -e '{}/.gdignore' \; \) \) -prune \
	-o -type f -name '*.gdextension' -print | sed 's|^\./|res://|' | LC_ALL=C sort)"
[ -n "$paths" ] || exit 0

mkdir -p "$dir/.godot"
printf '%s\n' "$paths" >"$list"
