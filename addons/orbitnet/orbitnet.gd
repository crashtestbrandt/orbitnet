@tool
extends EditorPlugin
## OrbitNet plugin -- registers the `Net` autoload (the netcode facade) while enabled. The autoload entry
## written into project.godot is what makes `Net` available in exported builds; the EditorPlugin itself is
## editor-only, so enabling/disabling the plugin adds/removes it.
##
## Rollback backend: the OrbitNet native Rust GDExtension -- sources in native/ (a cargo workspace at the
## repo root), binaries in the sibling addon addons/orbitnet_native/. The backend is reached ONLY through
## addons/orbitnet/net.gd; the `just net-check` grep gate (CI) fails if any other file references a backend
## class, so the rollback layer can be swapped without touching game code.
##
## BOTH addon directories must be installed together: this one is the GDScript surface, addons/orbitnet_native/
## carries the .gdextension and its binaries. `Net` without the extension is a facade over nothing.

const AUTOLOAD_NAME: String = "Net"
const AUTOLOAD_PATH: String = "res://addons/orbitnet/net.gd"

func _enter_tree() -> void:
	add_autoload_singleton(AUTOLOAD_NAME, AUTOLOAD_PATH)

func _exit_tree() -> void:
	remove_autoload_singleton(AUTOLOAD_NAME)
