//! Native crash capture that survives a RELEASE export template.
//!
//! Godot's own crash handler is `DEBUG_ENABLED`-only on every desktop platform
//! (`platform/linuxbsd/crash_handler_linuxbsd.cpp` `#ifndef DEBUG_ENABLED / #undef
//! CRASH_HANDLER_ENABLED`, `platform/windows/crash_handler_windows.h` `#if defined(DEBUG_ENABLED) /
//! #define CRASH_HANDLER_EXCEPTION`). A shipped build therefore prints **no** backtrace and never
//! posts `MainLoop::NOTIFICATION_CRASH`, so a game's own crash reporter never writes a report either
//! — the absence of both in a field log is the expected baseline, not a clue. That is exactly how the
//! death/respawn use-after-free arrived: three logs truncated mid-line with nothing after them.
//!
//! This extension is first-party and loads in every build, debug and release alike, so it can install
//! what the engine does not. What lands here is deliberately small: the signal/exception number and a
//! frame list, written with async-signal-safe calls only, appended to `<user>/logs/crash-native.log`.
//! Addresses without symbols are still actionable — keep the shipped cdylib, and
//! `addr2line -e liborbitnet.*.so <addr>` resolves them after the fact.
//!
//! ## What it does and does not catch
//!
//! * **Linux / macOS**: `SIGSEGV`, `SIGBUS`, `SIGILL`, `SIGFPE`, **and `SIGABRT`**. That last one is
//!   the point — glibc raises it when it detects heap corruption, which is how a write-after-free
//!   actually kills the process, and Godot's handler does not install it even in debug builds.
//!   `SA_ONSTACK` + a dedicated alt stack means a stack overflow is captured too.
//! * **Windows**: access violations and friends, via `SetUnhandledExceptionFilter`. It does **not**
//!   see `__fastfail` — what the CRT raises on detected heap corruption, and the Windows counterpart
//!   of the `SIGABRT` case above. A fail-fast bypasses every frame-based and vector-based handler by
//!   design, so nothing in-process can catch it. Only an out-of-process collector sees that one.
//!
//! ## Windows Error Reporting is READ, never written
//!
//! WER's `LocalDumps` is that out-of-process collector, and it does still run for a fail-fast.
//! OrbitNet does not configure it, and cannot:
//!
//! * All four `LocalDumps` values are documented HKLM-only — "This setting is not supported in the
//!   **HKEY_CURRENT_USER** registry hive" (WER Settings) — so there is no per-user hive to fall back
//!   to when the machine-wide one is out of reach.
//! * Writing the machine-wide key needs administrator privileges, and it sets crash-collection policy
//!   for **every** application on the machine, not just the one that called. A game process has no
//!   business holding either.
//!
//! So [`local_dumps`] READS the effective policy instead — the HKLM key, overridden by the per-image
//! subkey — and the facade republishes it. A crash report can then name the folder a dump would land
//! in (`<DumpFolder>\<image>.<pid>.dmp`), or say plainly that nothing collects. Setting the keys is
//! the consuming project's installer's job; `docs/crash-capture.md` carries them and this decision.
//!
//! Every handler chains: it restores the previous disposition and re-raises (POSIX) or returns
//! `EXCEPTION_CONTINUE_SEARCH` (Windows), so a debugger, a core dump, and Godot's own debug-build
//! handler all still get their turn. This only ever ADDS a record.

use std::sync::atomic::{AtomicBool, Ordering};

/// Frames captured per crash. Deep enough to cross the Godot/GDScript/extension boundary, small
/// enough to live on the (possibly alternate, possibly tiny) signal stack.
const MAX_FRAMES: usize = 64;

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// The report path, NUL-terminated and resolved at install time.
///
/// Built BEFORE any crash so the handler never allocates, never formats a path, and never calls into
/// Godot — all three are forbidden once a signal is in flight.
const LOG_PATH_BYTES: usize = 512;
static mut LOG_PATH: [u8; LOG_PATH_BYTES] = [0; LOG_PATH_BYTES];
static PATH_READY: AtomicBool = AtomicBool::new(false);

/// Install the handler, writing reports into `dir` (an absolute, already-created directory).
///
/// Returns false if the path does not fit, or if a handler is already installed. Best-effort by
/// construction: a failure here must never keep the game from starting.
pub(crate) fn install(dir: &str) -> bool {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return false;
    }
    let name = b"/crash-native.log";
    let bytes = dir.as_bytes();
    // SAFETY: single-threaded install (called once from the boot path, guarded by INSTALLED above),
    // and every later read happens after PATH_READY is set with Release ordering.
    unsafe {
        let path = (&raw mut LOG_PATH) as *mut u8;
        let len = bytes.len() + name.len();
        if len + 1 > LOG_PATH_BYTES {
            return false;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), path, bytes.len());
        std::ptr::copy_nonoverlapping(name.as_ptr(), path.add(bytes.len()), name.len());
        path.add(len).write(0);
    }
    PATH_READY.store(true, Ordering::Release);
    platform::install();
    true
}

// --- Windows Error Reporting: the fail-fast path -------------------------------------------------
//
// Read-only, and called from the game thread rather than from a handler, so the async-signal-safety
// rules the rest of this module lives under do not apply here: this half may allocate. The decision
// this implements -- OrbitNet reads WER's policy and never writes it -- is in the module header.

/// WER's own documented defaults, applied when the `LocalDumps` key exists but leaves a value unset.
/// Reproduced here because "the key is present with no values" is the common configuration, and a
/// report that says `dump_type 1` beats one that says the value was absent.
#[cfg(any(windows, test))]
const DEFAULT_DUMP_TYPE: i64 = 1;
#[cfg(any(windows, test))]
const DEFAULT_DUMP_COUNT: i64 = 10;
/// The documented default folder, unexpanded. Only ever reported when `%LOCALAPPDATA%` is not in the
/// environment, which on Windows means something is already wrong.
#[cfg(windows)]
const DEFAULT_DUMP_FOLDER: &str = r"%LOCALAPPDATA%\CrashDumps";

/// What one `LocalDumps` key sets. `None` per field means the key does not set it, which is what
/// makes the two-key merge below a per-VALUE override rather than a whole-key one -- WER reads the
/// global key first and then overrides individual values from the per-image subkey.
#[cfg(any(windows, test))]
#[derive(Default, Clone)]
pub(crate) struct DumpKey {
    folder: Option<String>,
    dump_type: Option<i64>,
    dump_count: Option<i64>,
}

/// The effective `LocalDumps` policy for THIS process.
///
/// `configured` is what a report should lead with: false means a fail-fast leaves nothing behind at
/// all, so the absence of a dump is the expected baseline rather than a lost file to hunt for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LocalDumps {
    /// False off Windows, where the question does not arise.
    pub(crate) supported: bool,
    /// Whether WER collects a dump for this process at all. The `LocalDumps` key's PRESENCE is what
    /// enables collection; its values only override the defaults.
    pub(crate) configured: bool,
    /// Which key decided it: `none`, `global`, or `image` for the per-executable subkey.
    pub(crate) scope: &'static str,
    /// Where a dump would land, environment-expanded. Empty when nothing collects.
    pub(crate) folder: String,
    /// 0 custom, 1 mini (WER's default), 2 full.
    pub(crate) dump_type: i64,
    /// How many dumps the folder keeps before the oldest is replaced.
    pub(crate) dump_count: i64,
    /// This process's executable file name -- the name WER matches a per-image subkey on, and the
    /// stem of the dump file (`<image>.<pid>.dmp`).
    pub(crate) image: String,
}

impl LocalDumps {
    /// Nothing collects: either the platform has no WER, or neither `LocalDumps` key exists.
    fn none(supported: bool, image: String) -> Self {
        Self {
            supported,
            configured: false,
            scope: "none",
            folder: String::new(),
            dump_type: 0,
            dump_count: 0,
            image,
        }
    }
}

/// Merge the global key with the per-image subkey the way WER does: global first, then each value the
/// per-image subkey sets. Either key on its own enables collection.
///
/// Pure, so it is a unit test on every platform rather than a Windows-only claim.
#[cfg(any(windows, test))]
fn resolve(
    global: Option<DumpKey>,
    per_image: Option<DumpKey>,
    image: &str,
    default_folder: &str,
) -> LocalDumps {
    if global.is_none() && per_image.is_none() {
        return LocalDumps::none(true, image.to_string());
    }
    let scope = if per_image.is_some() {
        "image"
    } else {
        "global"
    };
    let global = global.unwrap_or_default();
    let per_image = per_image.unwrap_or_default();
    LocalDumps {
        supported: true,
        configured: true,
        scope,
        folder: per_image
            .folder
            .or(global.folder)
            .unwrap_or_else(|| default_folder.to_string()),
        dump_type: per_image
            .dump_type
            .or(global.dump_type)
            .unwrap_or(DEFAULT_DUMP_TYPE),
        dump_count: per_image
            .dump_count
            .or(global.dump_count)
            .unwrap_or(DEFAULT_DUMP_COUNT),
        image: image.to_string(),
    }
}

/// The file name from a full image path. WER keys its per-image subkey on the file name alone, and
/// both separators are accepted because a path that reached us through Godot may carry either.
#[cfg(any(windows, test))]
fn image_base(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or("").to_string()
}

/// The effective WER `LocalDumps` policy for this process. Never writes the registry.
#[cfg(windows)]
pub(crate) fn local_dumps() -> LocalDumps {
    platform::local_dumps()
}

/// No WER off Windows -- and no gap either: the POSIX handler above already covers `SIGABRT`, which
/// is the case a fail-fast stands in for.
#[cfg(not(windows))]
pub(crate) fn local_dumps() -> LocalDumps {
    LocalDumps::none(false, String::new())
}

/// Append `bytes` to `fd`, retrying a short write. Async-signal-safe (write(2) only).
#[cfg(unix)]
unsafe fn write_all(fd: i32, bytes: &[u8]) {
    let mut off = 0usize;
    while off < bytes.len() {
        // SAFETY: fd is open, the slice is live for the call.
        let n = unsafe {
            libc::write(
                fd,
                bytes[off..].as_ptr() as *const libc::c_void,
                bytes.len() - off,
            )
        };
        if n <= 0 {
            return;
        }
        off += n as usize;
    }
}

/// Render `value` as decimal into `buf`, returning the written slice. No allocation, no formatting
/// machinery — `core::fmt` is not async-signal-safe.
fn render_u64(value: u64, buf: &mut [u8; 24]) -> &[u8] {
    if value == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let mut i = buf.len();
    let mut v = value;
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    &buf[i..]
}

#[cfg(unix)]
mod platform {
    use super::{render_u64, write_all, LOG_PATH, MAX_FRAMES, PATH_READY};
    use std::sync::atomic::Ordering;

    /// Signals worth a report. SIGABRT is the one Godot's own handler omits and the one a corrupted
    /// heap actually arrives on.
    const SIGNALS: [i32; 5] = [
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGILL,
        libc::SIGFPE,
        libc::SIGABRT,
    ];

    /// A dedicated stack so a stack-overflow SIGSEGV still has room to run the handler.
    const ALT_STACK_BYTES: usize = 64 * 1024;
    static mut ALT_STACK: [u8; ALT_STACK_BYTES] = [0; ALT_STACK_BYTES];

    /// Whatever was installed before us, parallel to [`SIGNALS`]. Restoring the PREVIOUS action rather
    /// than `SIG_DFL` is what makes this additive: in a debug build Godot has its own handler on
    /// SIGSEGV/SIGFPE/SIGILL, and blindly defaulting would trade its symbolicated backtrace for ours.
    static mut PREVIOUS: [libc::sigaction; SIGNALS.len()] = unsafe { std::mem::zeroed() };

    extern "C" {
        /// glibc / libSystem, `<execinfo.h>`. `backtrace_symbols_fd` is the async-signal-safe half of
        /// the pair (unlike `backtrace_symbols`, it does not malloc).
        fn backtrace(buffer: *mut *mut libc::c_void, size: libc::c_int) -> libc::c_int;
        fn backtrace_symbols_fd(
            buffer: *const *mut libc::c_void,
            size: libc::c_int,
            fd: libc::c_int,
        );
    }

    extern "C" fn handle(sig: libc::c_int) {
        if PATH_READY.load(Ordering::Acquire) {
            // SAFETY: LOG_PATH is NUL-terminated and immutable once PATH_READY is set. Everything in
            // this block is async-signal-safe: open/write/close/backtrace_symbols_fd.
            unsafe {
                let path = (&raw const LOG_PATH) as *const libc::c_char;
                let fd = libc::open(
                    path,
                    libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
                    0o644 as libc::c_uint,
                );
                if fd >= 0 {
                    write_all(fd, b"\n=== orbitnet native crash ===\nsignal: ");
                    let mut buf = [0u8; 24];
                    write_all(fd, render_u64(sig as u64, &mut buf));
                    write_all(fd, b"\npid: ");
                    let mut buf = [0u8; 24];
                    write_all(fd, render_u64(libc::getpid() as u64, &mut buf));
                    write_all(fd, b"\nframes:\n");
                    let mut frames = [std::ptr::null_mut::<libc::c_void>(); MAX_FRAMES];
                    let n = backtrace(frames.as_mut_ptr(), MAX_FRAMES as libc::c_int);
                    if n > 0 {
                        backtrace_symbols_fd(frames.as_ptr(), n, fd);
                    }
                    write_all(fd, b"=== end ===\n");
                    libc::close(fd);
                }
            }
        }
        // Chain: hand the signal back to whatever was there before -- Godot's debug-build handler, or
        // the default disposition that produces a core dump -- so this only ever ADDS a record.
        // SAFETY: PREVIOUS is written once at install, before any handler can run, and is only read
        // here. sigaction/raise are both async-signal-safe.
        unsafe {
            let previous = &raw const PREVIOUS;
            let mut restored = false;
            for (index, known) in SIGNALS.iter().enumerate() {
                if *known == sig {
                    libc::sigaction(sig, (*previous).as_ptr().add(index), std::ptr::null_mut());
                    restored = true;
                    break;
                }
            }
            if !restored {
                libc::signal(sig, libc::SIG_DFL);
            }
            libc::raise(sig);
        }
    }

    pub(super) fn install() {
        // SAFETY: called once, before any other thread can crash; every pointer is to a static.
        unsafe {
            // Warm the unwinder OUTSIDE signal context, BEFORE arming anything. glibc's first
            // backtrace() call can dlopen libgcc and malloc its unwind state; paying that lazily
            // inside the handler would deadlock in precisely the case this module exists for --
            // SIGABRT raised from a corrupted or already-locked malloc arena. Afterwards the
            // in-handler call touches no allocator.
            let mut warm = [std::ptr::null_mut::<libc::c_void>(); 8];
            backtrace(warm.as_mut_ptr(), warm.len() as libc::c_int);

            let mut stack: libc::stack_t = std::mem::zeroed();
            stack.ss_sp = (&raw mut ALT_STACK) as *mut libc::c_void;
            stack.ss_size = ALT_STACK_BYTES;
            stack.ss_flags = 0;
            libc::sigaltstack(&stack, std::ptr::null_mut());

            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handle as *const () as usize;
            action.sa_flags = libc::SA_ONSTACK | libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            let previous = (&raw mut PREVIOUS) as *mut libc::sigaction;
            for (index, sig) in SIGNALS.iter().enumerate() {
                libc::sigaction(*sig, &action, previous.add(index));
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{
        image_base, render_u64, resolve, DumpKey, LocalDumps, DEFAULT_DUMP_FOLDER, LOG_PATH,
        MAX_FRAMES, PATH_READY,
    };
    use std::sync::atomic::Ordering;

    type Handle = *mut core::ffi::c_void;
    const INVALID_HANDLE_VALUE: Handle = usize::MAX as Handle;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const OPEN_ALWAYS: u32 = 4;
    const FILE_APPEND_END: u32 = 2; // FILE_END, for SetFilePointer
    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

    #[repr(C)]
    struct ExceptionPointers {
        exception_record: *mut ExceptionRecord,
        context_record: *mut core::ffi::c_void,
    }

    /// The callback `SetUnhandledExceptionFilter` takes, spelled as a FUNCTION POINTER rather than as the
    /// `usize` the raw Win32 signature is often transcribed to. Rust 1.94 warns on casting a function item
    /// straight to an integer (`function_casts_as_integer`), and the cast was never buying anything: naming
    /// the real type lets the compiler check that `filter` still matches what the OS will call, which an
    /// integer parameter cannot. `Option<fn>` is the nullable form -- passing `None` clears the filter, and
    /// the previous one comes back the same way.
    type TopLevelExceptionFilter = extern "system" fn(*mut ExceptionPointers) -> i32;

    #[repr(C)]
    struct ExceptionRecord {
        exception_code: u32,
        exception_flags: u32,
        next: *mut ExceptionRecord,
        exception_address: *mut core::ffi::c_void,
        // The remaining fields are unused here.
    }

    extern "system" {
        fn CreateFileA(
            path: *const u8,
            access: u32,
            share: u32,
            security: *mut core::ffi::c_void,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn SetFilePointer(file: Handle, low: i32, high: *mut i32, method: u32) -> u32;
        fn WriteFile(
            file: Handle,
            buf: *const u8,
            len: u32,
            written: *mut u32,
            overlapped: *mut core::ffi::c_void,
        ) -> i32;
        fn CloseHandle(file: Handle) -> i32;
        fn GetCurrentProcessId() -> u32;
        fn RtlCaptureStackBackTrace(
            skip: u32,
            capture: u32,
            frames: *mut *mut core::ffi::c_void,
            hash: *mut u32,
        ) -> u16;
        fn SetUnhandledExceptionFilter(
            filter: Option<TopLevelExceptionFilter>,
        ) -> Option<TopLevelExceptionFilter>;
        fn GetModuleFileNameW(module: Handle, buf: *mut u16, size: u32) -> u32;
    }

    unsafe fn write_all(file: Handle, bytes: &[u8]) {
        let mut written: u32 = 0;
        // SAFETY: handle is open, slice is live for the call.
        unsafe {
            WriteFile(
                file,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            );
        }
    }

    unsafe fn write_hex(file: Handle, value: usize) {
        let mut buf = [0u8; 18];
        buf[0] = b'0';
        buf[1] = b'x';
        for i in 0..16 {
            let nibble = ((value >> ((15 - i) * 4)) & 0xf) as u8;
            buf[2 + i] = if nibble < 10 {
                b'0' + nibble
            } else {
                b'a' + nibble - 10
            };
        }
        // SAFETY: delegated to write_all's contract.
        unsafe { write_all(file, &buf) };
    }

    extern "system" fn filter(info: *mut ExceptionPointers) -> i32 {
        if PATH_READY.load(Ordering::Acquire) && !info.is_null() {
            // SAFETY: the OS hands us a valid ExceptionPointers; LOG_PATH is NUL-terminated and
            // immutable once PATH_READY is set.
            unsafe {
                let file = CreateFileA(
                    (&raw const LOG_PATH) as *const u8,
                    GENERIC_WRITE,
                    FILE_SHARE_READ,
                    std::ptr::null_mut(),
                    OPEN_ALWAYS,
                    0,
                    std::ptr::null_mut(),
                );
                if file != INVALID_HANDLE_VALUE {
                    SetFilePointer(file, 0, std::ptr::null_mut(), FILE_APPEND_END);
                    write_all(file, b"\n=== orbitnet native crash ===\ncode: ");
                    let record = (*info).exception_record;
                    let code = if record.is_null() {
                        0
                    } else {
                        (*record).exception_code as usize
                    };
                    write_hex(file, code);
                    write_all(file, b"\npid: ");
                    let mut buf = [0u8; 24];
                    write_all(file, render_u64(GetCurrentProcessId() as u64, &mut buf));
                    write_all(file, b"\nframes:\n");
                    let mut frames = [std::ptr::null_mut::<core::ffi::c_void>(); MAX_FRAMES];
                    let n = RtlCaptureStackBackTrace(
                        0,
                        MAX_FRAMES as u32,
                        frames.as_mut_ptr(),
                        std::ptr::null_mut(),
                    );
                    for frame in frames.iter().take(n as usize) {
                        write_hex(file, *frame as usize);
                        write_all(file, b"\n");
                    }
                    write_all(file, b"=== end ===\n");
                    CloseHandle(file);
                }
            }
        }
        // Fall through to the default handling / any attached debugger. NOTE: this filter is never
        // reached for __fastfail (detected heap corruption), which bypasses SEH by design — that case
        // needs an out-of-process collector.
        EXCEPTION_CONTINUE_SEARCH
    }

    pub(super) fn install() {
        // SAFETY: called once from the boot path.
        unsafe { SetUnhandledExceptionFilter(Some(filter)) };
    }

    // --- WER LocalDumps read-back ---------------------------------------------------------------

    type HKey = *mut core::ffi::c_void;
    const HKEY_LOCAL_MACHINE: HKey = 0x8000_0002u32 as usize as HKey;
    const ERROR_SUCCESS: i32 = 0;
    const ERROR_MORE_DATA: i32 = 234;
    /// `KEY_READ | KEY_WOW64_64KEY`. The 64-bit view is named explicitly so a 32-bit export reads the
    /// key WER itself consults rather than the `Wow6432Node` mirror; on a 32-bit OS the flag is
    /// ignored.
    const KEY_READ_64: u32 = 0x0002_0019 | 0x0000_0100;
    /// `RRF_RT_ANY`, with the returned type checked against [`REG_SZ`] / [`REG_EXPAND_SZ`] instead.
    ///
    /// `RRF_NOEXPAND` is deliberately ABSENT: `DumpFolder` is a `REG_EXPAND_SZ` holding
    /// `%LOCALAPPDATA%\CrashDumps` by default, and the expansion has to happen in THIS process's
    /// environment, because this process is the one that would crash. Expansion turns the reported
    /// type into `REG_SZ`, which is exactly what a type-restricting flag combination gets wrong -- so
    /// the restriction is dropped and the type is inspected after the fact.
    const RRF_ANY: u32 = 0x0000_ffff;
    const REG_SZ: u32 = 1;
    const REG_EXPAND_SZ: u32 = 2;
    const RRF_DWORD: u32 = 0x0000_0010;
    /// The machine-wide key. There is no `HKEY_CURRENT_USER` counterpart -- see the module header.
    const LOCAL_DUMPS_KEY: &str = r"SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps";

    #[link(name = "advapi32")]
    extern "system" {
        fn RegOpenKeyExW(
            key: HKey,
            sub_key: *const u16,
            options: u32,
            desired: u32,
            out: *mut HKey,
        ) -> i32;
        fn RegCloseKey(key: HKey) -> i32;
        fn RegGetValueW(
            key: HKey,
            sub_key: *const u16,
            value: *const u16,
            flags: u32,
            kind: *mut u32,
            data: *mut core::ffi::c_void,
            len: *mut u32,
        ) -> i32;
    }

    /// A NUL-terminated UTF-16 copy, which is what every `W` entry point wants.
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// One string value, environment-expanded. `None` if it is absent or is not a string.
    unsafe fn read_string(key: HKey, name: &str) -> Option<String> {
        let name = wide(name);
        let mut bytes: u32 = 0;
        // SAFETY: `key` is open for KEY_QUERY_VALUE; a null data pointer asks for the size alone.
        let status = unsafe {
            RegGetValueW(
                key,
                std::ptr::null(),
                name.as_ptr(),
                RRF_ANY,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut bytes,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        // Two attempts rather than one: the size a probing call reports is the UNEXPANDED string's,
        // and `%LOCALAPPDATA%` expands longer than it reads. A short buffer comes back as
        // ERROR_MORE_DATA carrying the real size, which the second attempt allocates to.
        for _ in 0..2 {
            let mut buf = vec![0u16; (bytes as usize / 2) + 1];
            let mut size = (buf.len() * 2) as u32;
            let mut kind: u32 = 0;
            // SAFETY: the buffer is live and `size` states its true length in bytes.
            let status = unsafe {
                RegGetValueW(
                    key,
                    std::ptr::null(),
                    name.as_ptr(),
                    RRF_ANY,
                    &mut kind,
                    buf.as_mut_ptr().cast(),
                    &mut size,
                )
            };
            if status == ERROR_SUCCESS {
                if kind != REG_SZ && kind != REG_EXPAND_SZ {
                    return None;
                }
                // RegGetValue counts the terminator; a registry string may also be stored without
                // one, so clamp rather than trusting the arithmetic.
                let chars = ((size as usize / 2).saturating_sub(1)).min(buf.len());
                return Some(String::from_utf16_lossy(&buf[..chars]));
            }
            if status != ERROR_MORE_DATA {
                return None;
            }
            bytes = size;
        }
        None
    }

    /// One `REG_DWORD` value. `None` if it is absent or is not a DWORD.
    unsafe fn read_dword(key: HKey, name: &str) -> Option<i64> {
        let name = wide(name);
        let mut value: u32 = 0;
        let mut size: u32 = 4;
        // SAFETY: `key` is open for KEY_QUERY_VALUE; the destination is a live u32 and `size` says so.
        let status = unsafe {
            RegGetValueW(
                key,
                std::ptr::null(),
                name.as_ptr(),
                RRF_DWORD,
                std::ptr::null_mut(),
                (&raw mut value).cast(),
                &mut size,
            )
        };
        (status == ERROR_SUCCESS).then_some(i64::from(value))
    }

    /// The three values one `LocalDumps` key sets, or `None` if the key does not exist. The
    /// distinction matters: an EMPTY key still enables collection, at WER's defaults.
    unsafe fn read_key(sub_key: &str) -> Option<DumpKey> {
        let sub_key = wide(sub_key);
        let mut key: HKey = std::ptr::null_mut();
        // SAFETY: both pointers are to live locals; the string is NUL-terminated.
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                sub_key.as_ptr(),
                0,
                KEY_READ_64,
                &mut key,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        // SAFETY: `key` is open until the RegCloseKey below.
        let values = unsafe {
            DumpKey {
                folder: read_string(key, "DumpFolder"),
                dump_type: read_dword(key, "DumpType"),
                dump_count: read_dword(key, "DumpCount"),
            }
        };
        // SAFETY: closing the handle this function opened, exactly once.
        unsafe { RegCloseKey(key) };
        Some(values)
    }

    /// This process's executable file name, which is what WER matches a per-image subkey on.
    fn image_name() -> String {
        let mut buf = [0u16; 1024];
        // SAFETY: a null module handle asks for this process's own image; `buf.len()` bounds the write.
        let written =
            unsafe { GetModuleFileNameW(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) }
                as usize;
        image_base(&String::from_utf16_lossy(&buf[..written.min(buf.len())]))
    }

    /// WER's default folder, expanded here rather than reported as the literal the documentation
    /// gives: a report that names a path a player can paste into Explorer is worth more than one that
    /// names an environment variable.
    fn default_folder() -> String {
        match std::env::var("LOCALAPPDATA") {
            Ok(local) if !local.is_empty() => format!(r"{local}\CrashDumps"),
            _ => DEFAULT_DUMP_FOLDER.to_string(),
        }
    }

    pub(super) fn local_dumps() -> LocalDumps {
        let image = image_name();
        // SAFETY: both calls only read; each opens and closes its own key.
        let global = unsafe { read_key(LOCAL_DUMPS_KEY) };
        let per_image = if image.is_empty() {
            None
        } else {
            unsafe { read_key(&format!(r"{LOCAL_DUMPS_KEY}\{image}")) }
        };
        resolve(global, per_image, &image, &default_folder())
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    pub(super) fn install() {}
}

#[cfg(test)]
mod tests {
    use super::{image_base, local_dumps, render_u64, resolve, DumpKey};

    #[test]
    fn renders_decimal_without_allocating() {
        let mut buf = [0u8; 24];
        assert_eq!(render_u64(0, &mut buf), b"0");
        let mut buf = [0u8; 24];
        assert_eq!(render_u64(11, &mut buf), b"11");
        let mut buf = [0u8; 24];
        assert_eq!(render_u64(1234567890, &mut buf), b"1234567890");
        let mut buf = [0u8; 24];
        assert_eq!(render_u64(u64::MAX, &mut buf), b"18446744073709551615");
    }

    /// The three values a key sets, spelled out at each call site so a test reads as the registry
    /// state it stands for.
    fn key(folder: Option<&str>, dump_type: Option<i64>, dump_count: Option<i64>) -> DumpKey {
        DumpKey {
            folder: folder.map(str::to_string),
            dump_type,
            dump_count,
        }
    }

    #[test]
    fn no_local_dumps_key_means_nothing_collects() {
        let dumps = resolve(
            None,
            None,
            "game.exe",
            r"C:\Users\ada\AppData\Local\CrashDumps",
        );
        assert!(!dumps.configured);
        assert_eq!(dumps.scope, "none");
        // Empty rather than the default folder: naming a folder nothing writes to would send a
        // player hunting for a file that was never created.
        assert_eq!(dumps.folder, "");
        assert_eq!(dumps.image, "game.exe");
    }

    #[test]
    fn an_empty_key_still_collects_at_wers_defaults() {
        let dumps = resolve(
            Some(key(None, None, None)),
            None,
            "game.exe",
            r"C:\Users\ada\AppData\Local\CrashDumps",
        );
        assert!(
            dumps.configured,
            "the key's PRESENCE is what enables collection"
        );
        assert_eq!(dumps.scope, "global");
        assert_eq!(dumps.folder, r"C:\Users\ada\AppData\Local\CrashDumps");
        assert_eq!(dumps.dump_type, 1);
        assert_eq!(dumps.dump_count, 10);
    }

    #[test]
    fn the_per_image_subkey_overrides_value_by_value() {
        let dumps = resolve(
            Some(key(Some(r"D:\dumps"), Some(1), Some(10))),
            Some(key(None, Some(2), None)),
            "game.exe",
            r"C:\Users\ada\AppData\Local\CrashDumps",
        );
        assert_eq!(dumps.scope, "image");
        // Folder and count fall through from the global key -- WER reads that one first and then
        // overrides only the values the per-image subkey actually sets.
        assert_eq!(dumps.folder, r"D:\dumps");
        assert_eq!(dumps.dump_count, 10);
        assert_eq!(dumps.dump_type, 2, "a full dump was asked for per image");
    }

    #[test]
    fn a_per_image_subkey_alone_collects() {
        let dumps = resolve(
            None,
            Some(key(Some(r"D:\dumps"), None, None)),
            "game.exe",
            r"C:\Users\ada\AppData\Local\CrashDumps",
        );
        assert!(dumps.configured);
        assert_eq!(dumps.scope, "image");
        assert_eq!(dumps.folder, r"D:\dumps");
        assert_eq!(dumps.dump_type, 1);
    }

    #[test]
    fn the_image_name_is_the_file_name_either_separator() {
        assert_eq!(image_base(r"C:\Program Files\Game\game.exe"), "game.exe");
        assert_eq!(image_base("/opt/game/game.x86_64"), "game.x86_64");
        assert_eq!(image_base("game.exe"), "game.exe");
        assert_eq!(image_base(""), "");
    }

    #[test]
    fn the_readback_never_claims_a_dump_folder_it_did_not_find() {
        // Holds on every platform, and is the invariant a crash report reads: a folder is reported
        // only when something is configured to write into it. Off Windows both are false/empty.
        let dumps = local_dumps();
        assert_eq!(
            dumps.configured,
            !dumps.folder.is_empty(),
            "configured and a named folder travel together"
        );
        if !cfg!(windows) {
            assert!(!dumps.supported, "WER is a Windows question");
        }
    }
}
