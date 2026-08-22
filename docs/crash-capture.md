# Native crash capture

What a shipped build records when it dies, and the one crash no in-process handler can see.

Godot's own crash handler is `DEBUG_ENABLED`-only on every desktop platform, and so is
`MainLoop::NOTIFICATION_CRASH`. A **release export template therefore prints no backtrace and posts no
notification** — a game's own crash reporter never runs either. The absence of both in a field log is the
expected baseline, not a clue.

The OrbitNet extension loads in every build, debug and release alike, so it installs what the engine does not.

```gdscript
# Once, at boot. `dir` must be an absolute, already-created directory.
DirAccess.make_dir_recursive_absolute("user://logs")
Net.install_native_crash_handler(ProjectSettings.globalize_path("user://logs"))
```

Reports are **appended** to `<dir>/crash-native.log`: the signal or exception number, the pid, and up to 64
frames, written with async-signal-safe calls only. Every handler chains — it restores the previous
disposition and re-raises (POSIX) or returns `EXCEPTION_CONTINUE_SEARCH` (Windows) — so a debugger, a core
dump and Godot's own debug-build handler all still get their turn. Installing only ever ADDS a record.

Addresses arrive unsymbolized. Keep the shipped cdylib and resolve them afterwards:

```sh
addr2line -e liborbitnet.linux.template_release.x86_64.so 0x1a2b3c
```

## What each platform catches

| Platform | Caught | Mechanism |
|---|---|---|
| Linux, macOS | `SIGSEGV`, `SIGBUS`, `SIGILL`, `SIGFPE`, `SIGABRT` | `sigaction` on a dedicated alt stack (`SA_ONSTACK`), so a stack-overflow `SIGSEGV` is captured too |
| Windows | Access violations and friends | `SetUnhandledExceptionFilter` |
| Windows | **`__fastfail` — not caught** | Bypasses every in-process handler; see below |

`SIGABRT` is the one Godot's handler omits even in debug builds, and it is the one that matters: glibc raises
it when it detects heap corruption, which is how a write-after-free actually kills a process.

## The Windows fail-fast gap

`__fastfail` is what the CRT raises on detected heap corruption — the Windows counterpart of that `SIGABRT`
case. It **bypasses every frame-based and vector-based exception handler by design**, so
`SetUnhandledExceptionFilter` never runs for it and nothing in-process can catch it. A fail-fast leaves
`crash-native.log` with no new record at all.

The only collector that sees it is out-of-process: Windows Error Reporting's `LocalDumps`, which still runs
for a fail-fast because it is not an exception handler.

## OrbitNet reads WER's policy and never writes it

**The addon does not configure `LocalDumps`, and cannot.**

- All four values are documented **HKLM-only**: *"This setting is not supported in the `HKEY_CURRENT_USER`
  registry hive."* There is no per-user hive to fall back to.
- Writing the machine-wide key needs **administrator privileges**, which a game process has no business
  holding.
- The key sets crash-collection policy for **every application on the machine**, not just the one that wrote
  it. That is an installer's decision, made once, with consent — not a side effect of launching a game.

So the addon reads the effective policy back instead:

```gdscript
var dumps: Dictionary[String, Variant] = Net.native_crash_dump_config()
if dumps["configured"]:
    push_warning("a fail-fast would leave a dump in %s" % dumps["folder"])
else:
    push_warning("nothing collects a Windows fail-fast on this machine")
```

| Key | Meaning |
|---|---|
| `supported` | False off Windows, and on a backend binary too old to answer. The question does not arise. |
| `configured` | Whether WER collects a dump for this process at all. **False means a fail-fast leaves nothing.** |
| `scope` | Which key decided it: `none`, `global`, or `image` for the per-executable subkey |
| `folder` | Where a dump would land, environment-expanded. **Empty whenever nothing collects.** |
| `dump_type` | 0 custom, 1 mini (WER's default), 2 full |
| `dump_count` | How many dumps the folder keeps before the oldest is replaced |
| `image` | This process's executable file name, which is also the dump file's stem |

A dump lands at `<folder>\<image>.<pid>.dmp`. Put those figures in the report a game already writes, and an
investigator either has a dump to ask for or knows there was never going to be one.

## The registry keys, for a consumer's installer

Machine-wide, under `HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps`. The
key's **presence** is what enables collection; the values only override the defaults. A per-executable subkey
named after the image (`LocalDumps\MyGame.exe`) overrides the global key **value by value** — WER reads the
global settings first and then applies whichever values the subkey sets.

| Value | Type | Default | Meaning |
|---|---|---|---|
| `DumpFolder` | `REG_EXPAND_SZ` | `%LOCALAPPDATA%\CrashDumps` | Where dumps are written. A non-default folder needs an ACL the crashing process can write to. |
| `DumpCount` | `REG_DWORD` | 10 | Maximum dumps kept; the oldest is replaced past it. |
| `DumpType` | `REG_DWORD` | 1 | 0 custom, 1 mini, 2 full. |
| `CustomDumpFlags` | `REG_DWORD` | `0x121` | `MINIDUMP_TYPE` bits, used only when `DumpType` is 0. |

From an elevated prompt, for one executable:

```bat
reg add "HKLM\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\MyGame.exe" /v DumpType /t REG_DWORD /d 2 /f
```

Two caveats worth stating to whoever ships the installer:

- A **full** dump (`DumpType` 2) is the whole address space. On a game that is routinely a gigabyte or more
  per crash, and `DumpCount` is how many of those the player's disk keeps.
- A dump is a **memory image**: it carries whatever the process held, which can include player names, chat,
  and session tokens. Collect it deliberately, and say so where the player can read it.

## Where the decision lives

The reasoning above is repeated in the header comment of
`native/crates/orbitnet-godot/src/crash.rs`, which is what governs the code. Change one, change the other in
the same commit.

Reference: Microsoft, [Collecting User-Mode Dumps](https://learn.microsoft.com/en-us/windows/win32/wer/collecting-user-mode-dumps)
and [WER Settings](https://learn.microsoft.com/en-us/windows/win32/wer/wer-settings).
