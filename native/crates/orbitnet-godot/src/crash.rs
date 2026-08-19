//! Native crash capture that survives a RELEASE export template.
//!
//! Godot's own crash handler is `DEBUG_ENABLED`-only on every desktop platform
//! (`platform/linuxbsd/crash_handler_linuxbsd.cpp` `#ifndef DEBUG_ENABLED / #undef
//! CRASH_HANDLER_ENABLED`, `platform/windows/crash_handler_windows.h` `#if defined(DEBUG_ENABLED) /
//! #define CRASH_HANDLER_EXCEPTION`). A shipped build therefore prints **no** backtrace and never
//! posts `MainLoop::NOTIFICATION_CRASH`, so `CrashLogger` never writes a report either — the absence
//! of both in a field log is the expected baseline, not a clue. That is exactly how the death/respawn
//! use-after-free arrived: three logs truncated mid-line with nothing after them.
//!
//! This extension is first-party and loads in every build, debug and release alike, so it can install
//! what the engine does not. What lands here is deliberately small: the signal/exception number and a
//! frame list, written with async-signal-safe calls only, appended to `<user>/logs/crash-native.log`.
//! Addresses without symbols are still actionable — the shipped cdylib is in Git LFS, so
//! `addr2line -e liborbitnet.*.so <addr>` resolves them after the fact.
//!
//! ## What it does and does not catch
//!
//! * **Linux / macOS**: `SIGSEGV`, `SIGBUS`, `SIGILL`, `SIGFPE`, **and `SIGABRT`**. That last one is
//!   the point — glibc raises it when it detects heap corruption, which is how a write-after-free
//!   actually kills the process, and Godot's handler does not install it even in debug builds.
//!   `SA_ONSTACK` + a dedicated alt stack means a stack overflow is captured too.
//! * **Windows**: access violations and friends, via `SetUnhandledExceptionFilter`. It does **not**
//!   see `__fastfail` — which is what the CRT raises on detected heap corruption, and it bypasses SEH
//!   and unhandled-exception filters by design. Capturing that one needs an out-of-process collector
//!   (WER `LocalDumps`); see the README's "Native crash capture in a SHIPPED build" note.
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
    use super::{render_u64, LOG_PATH, MAX_FRAMES, PATH_READY};
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
}

#[cfg(not(any(unix, windows)))]
mod platform {
    pub(super) fn install() {}
}

#[cfg(test)]
mod tests {
    use super::render_u64;

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
}
