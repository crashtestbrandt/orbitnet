extends RefCounted
class_name InputTape
## Record / replay of a client's per-tick input stream for netbench -- the "record real sessions as
## regression assets" layer (capture the exact per-tick inputs, replay them later under impairment). A HUMAN
## (or a bot) plays; every frame the body actually consumed is appended to a tape; later the tape is replayed
## through the SAME [method BenchSubject.apply_input] seam under each network profile, so a recorded session
## becomes a reproducible input fixture the whole fleet can re-run.
##
## PURE: the encode/decode is a lossless [method @GlobalScope.var_to_bytes] round-trip, and save/load are
## thin FileAccess wrappers around it. No scene or socket dependency -- the live CAPTURE (reading the body's
## per-tick frame) and REPLAY (feeding frames back) live in [BenchProbe], which owns the timing.
##
## A frame is a plain Dictionary in the [BenchSubject] neutral vocabulary. That is what makes a tape
## GAME-AGNOSTIC and what makes this codec four lines instead of a hand-maintained field list: there is no
## per-game input class to mirror, so a field added to the vocabulary needs no change here, and a tape
## recorded before that field existed still loads -- the missing key simply keeps the subject's own default.
##
## FORMAT: a self-describing blob -- {magic, version, frames: Array[Dictionary]} through var_to_bytes.

const FORMAT_VERSION: int = 2   # v1 carried a game-specific input struct; v2 is the neutral vocabulary
const _MAGIC: String = "obnt"   # OrbitNet tape -- a header key so a stray non-tape file is rejected, not mis-parsed

var _frames: Array[Dictionary] = []

## Append one frame to the tape. The dictionary is DUPLICATED, so a caller that reuses one frame object
## across ticks (the common case -- a body holding a single input buffer) cannot retroactively rewrite the
## recording. Empty frames are dropped: an empty frame is the vocabulary's "release" signal, not input.
func record(frame: Dictionary) -> void:
	if frame.is_empty():
		return
	_frames.push_back(frame.duplicate(true))

## The recorded frames, in order.
func frames() -> Array[Dictionary]:
	return _frames

## The frame at `index`, or an EMPTY dictionary when out of range. The replay driver walks indices tick by
## tick and treats empty as "tape exhausted" -- which is also the release signal, so an exhausted tape hands
## the body back to live input by construction.
func frame_at(index: int) -> Dictionary:
	if index < 0 or index >= _frames.size():
		return {}
	return _frames[index]

func length() -> int:
	return _frames.size()

# --- lossless codec (pure) -----------------------------------------------------------------------
## Encode the whole tape to bytes. Round-trips losslessly through [method decode].
func encode() -> PackedByteArray:
	var blob: Dictionary = {"magic": _MAGIC, "version": FORMAT_VERSION, "frames": _frames}
	return var_to_bytes(blob)

## Decode a tape from bytes into THIS instance's frame list (replacing any current contents). Returns true on
## a well-formed tape, false (and leaves the frames empty) on a corrupt / non-tape blob.
##
## Uses var_to_bytes WITHOUT object support, so a hostile or corrupt file cannot instantiate anything -- a
## tape is data, and a bench artifact is exactly the sort of file that gets passed around between machines.
func decode(bytes: PackedByteArray) -> bool:
	_frames = []
	if bytes.is_empty():
		return false
	var v: Variant = bytes_to_var(bytes)
	if not (v is Dictionary):
		return false
	var blob: Dictionary = v
	if blob.get("magic", "") != _MAGIC:
		return false
	var raw_frames: Variant = blob.get("frames", [])
	if not (raw_frames is Array):
		return false
	var arr: Array = raw_frames
	for entry: Variant in arr:
		if entry is Dictionary:
			var d: Dictionary = entry
			_frames.push_back(d)
	return true

## Save the tape to `path` (creates parent dirs under user:// as needed). Returns OK or a FileAccess error.
func save(path: String) -> Error:
	var dir: String = path.get_base_dir()
	if dir != "" and not DirAccess.dir_exists_absolute(dir):
		DirAccess.make_dir_recursive_absolute(dir)
	var file: FileAccess = FileAccess.open(path, FileAccess.WRITE)
	if file == null:
		return FileAccess.get_open_error()
	file.store_buffer(encode())
	file.close()
	return OK

## Load a tape from `path` into this instance. Returns OK, ERR_FILE_NOT_FOUND, or ERR_FILE_CORRUPT.
func load_from(path: String) -> Error:
	if not FileAccess.file_exists(path):
		return ERR_FILE_NOT_FOUND
	var file: FileAccess = FileAccess.open(path, FileAccess.READ)
	if file == null:
		return FileAccess.get_open_error()
	var bytes: PackedByteArray = file.get_buffer(file.get_length())
	file.close()
	return OK if decode(bytes) else ERR_FILE_CORRUPT
