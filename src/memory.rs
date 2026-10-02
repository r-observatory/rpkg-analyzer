// What happens when memory runs out: the run ends, whoever was asking. And
// the stack a run has, which is memory it asks for before anything else.

use std::alloc::{GlobalAlloc, Layout, System, handle_alloc_error};

/// The system allocator, except that a request it cannot meet ends the run.
///
/// Reading a whole file reserves in a way that is allowed to fail, and the
/// failure comes back as an error like any other. A file there was no room
/// for then looked like a file that could not be read, and the record said
/// it was absent or damaged.
pub struct EndsRun;

// Each call stays out of line, as a call to the allocator was before. Inlined
// into their callers they made the data reader's frame larger, and that
// frame is paid once for every level of a nested object.
unsafe impl GlobalAlloc for EndsRun {
    #[inline(never)]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        made(unsafe { System.alloc(layout) }, layout)
    }
    #[inline(never)]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        made(unsafe { System.alloc_zeroed(layout) }, layout)
    }
    #[inline(never)]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let grown = Layout::from_size_align(new_size, layout.align()).unwrap_or(layout);
        made(unsafe { System.realloc(ptr, layout, new_size) }, grown)
    }
    #[inline(never)]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// What the system allocator handed back, unless it handed back nothing.
#[inline]
fn made(p: *mut u8, layout: Layout) -> *mut u8 {
    if p.is_null() {
        handle_alloc_error(layout)
    }
    p
}

/// Ends the run for memory that was asked for past the allocator above and
/// not given. The decoders are C libraries that ask the system themselves,
/// and so is the library that opens a directory.
#[cold]
pub fn out_of_memory(whose: &str) -> ! {
    eprintln!("memory allocation failed {whose}");
    std::process::abort()
}

/// How much stack a run has. The data reader calls itself once for every
/// level of a nested object, so the stack decides how deep a file is read,
/// and on the first thread's 8 MiB that came down to the size of one call,
/// which moves with every change to the reader and with the compiler.
pub const STACK: usize = 64 << 20;

/// Runs `body` on a thread of its own with `STACK` bytes of stack and waits
/// for it. The thread has the first thread's name, so a panic reads as it
/// did, and ends the run with the status it had.
pub fn on_a_deep_stack(body: impl FnOnce() + Send + 'static) {
    on_a_stack_of(STACK, body)
}

/// The same on a stack of `bytes`. A stack there is no room for ends the run
/// as any other memory that is not there does.
fn on_a_stack_of(bytes: usize, body: impl FnOnce() + Send + 'static) {
    keep_one_heap();
    let made = std::thread::Builder::new()
        .name("main".into())
        .stack_size(bytes)
        .spawn(body);
    let run = match made {
        Ok(run) => run,
        Err(why) => out_of_memory(&format!("for the stack of the run: {why}")),
    };
    // The panic has been reported by the thread it happened on.
    if run.join().is_err() {
        std::process::exit(101);
    }
}

/// glibc gives every thread but the first a heap of its own and reserves
/// address space for it 64 MiB at a time, 128 MiB while it finds a place
/// for it. With one heap for all threads the run allocates where it did on
/// the first thread, and a small package's address space grows by the stack
/// and nothing else.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn keep_one_heap() {
    use std::ffi::c_int;
    unsafe extern "C" {
        fn mallopt(param: c_int, value: c_int) -> c_int;
    }
    const M_ARENA_MAX: c_int = -8;
    unsafe { mallopt(M_ARENA_MAX, 1) };
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn keep_one_heap() {}

/// The entries of a directory. Opening one allocates inside the C library,
/// and with no memory there the error that comes back reads to every caller
/// like a directory that does not exist.
pub fn read_dir(dir: impl AsRef<std::path::Path>) -> std::io::Result<std::fs::ReadDir> {
    found(std::fs::read_dir(dir))
}

/// What the system answered, unless it answered that it had no memory.
fn found<T>(answer: std::io::Result<T>) -> std::io::Result<T> {
    let no_memory = |e: &std::io::Error| e.kind() == std::io::ErrorKind::OutOfMemory;
    if answer.as_ref().is_err_and(no_memory) {
        out_of_memory("opening a directory");
    }
    answer
}

/// A test that has to watch a run end runs again in a process of its own.
#[cfg(all(test, unix))]
pub mod child {
    const KEY: &str = "RPKG_ANALYZER_TEST_CHILD";

    /// Whether this process is the one `aborted` started for `test`.
    pub fn is(test: &str) -> bool {
        std::env::var(KEY).is_ok_and(|v| v == test)
    }

    /// Runs the one test of this name, given by its full path, in a new
    /// process, and returns how it ended and what it wrote.
    pub fn ran(test: &str) -> std::process::Output {
        std::process::Command::new(std::env::current_exe().expect("this test binary"))
            .args(["--exact", test, "--nocapture", "--test-threads", "1"])
            .env(KEY, test)
            .output()
            .expect("run the test again")
    }

    /// The same for a process that has to be aborted, which is how a failed
    /// allocation ends one. Returns what it wrote to stderr.
    pub fn aborted(test: &str) -> String {
        use std::os::unix::process::ExitStatusExt;
        let out = ran(test);
        let said = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(
            out.status.signal(),
            Some(6),
            "the run went on: {:?}\n{}\n{said}",
            out.status,
            String::from_utf8_lossy(&out.stdout)
        );
        said
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// More than any machine has, and still a size an allocation may ask for.
    #[cfg(unix)]
    const TOO_MUCH: usize = usize::MAX / 4;

    #[cfg(unix)]
    #[test]
    fn a_reserve_that_may_fail_ends_the_run() {
        const ME: &str = "memory::tests::a_reserve_that_may_fail_ends_the_run";
        if child::is(ME) {
            // What reading a whole file does before it reads any of it.
            let mut held: Vec<u8> = Vec::new();
            let got = held.try_reserve_exact(TOO_MUCH);
            println!("the reserve came back: {got:?}");
            return;
        }
        let said = child::aborted(ME);
        assert!(
            said.contains(&format!("memory allocation of {TOO_MUCH} bytes failed")),
            "{said}"
        );
    }

    /// A buffer that already holds something grows through another call.
    #[cfg(unix)]
    #[test]
    fn a_buffer_that_cannot_grow_ends_the_run() {
        const ME: &str = "memory::tests::a_buffer_that_cannot_grow_ends_the_run";
        if child::is(ME) {
            let mut held: Vec<u8> = vec![7; 64];
            let got = held.try_reserve_exact(TOO_MUCH);
            println!("the reserve came back: {got:?}");
            return;
        }
        let said = child::aborted(ME);
        assert!(said.contains("memory allocation of"), "{said}");
    }

    /// And memory asked for already zeroed comes through a third.
    #[cfg(unix)]
    #[test]
    fn a_zeroed_request_that_cannot_be_met_ends_the_run() {
        const ME: &str = "memory::tests::a_zeroed_request_that_cannot_be_met_ends_the_run";
        if child::is(ME) {
            let layout = Layout::from_size_align(TOO_MUCH, 1).expect("a layout");
            let got = unsafe { EndsRun.alloc_zeroed(layout) };
            println!("the request came back: {got:?}");
            return;
        }
        let said = child::aborted(ME);
        assert!(
            said.contains(&format!("memory allocation of {TOO_MUCH} bytes failed")),
            "{said}"
        );
    }

    /// The error the C library gives when it has no memory to open a
    /// directory with.
    #[cfg(unix)]
    #[test]
    fn a_directory_there_is_no_memory_to_open_ends_the_run() {
        const ME: &str = "memory::tests::a_directory_there_is_no_memory_to_open_ends_the_run";
        if child::is(ME) {
            let got = found::<()>(Err(std::io::Error::from_raw_os_error(12)));
            println!("the listing came back: {got:?}");
            return;
        }
        let said = child::aborted(ME);
        assert!(
            said.contains("memory allocation failed opening a directory"),
            "{said}"
        );
    }

    /// Goes down the stack a kilobyte or more at a call until it is `until`
    /// bytes from `base`, and returns how far that was.
    #[cfg(unix)]
    #[inline(never)]
    fn stack_used(until: usize, base: usize) -> usize {
        let held = std::hint::black_box([0u8; 1024]);
        let used = base.abs_diff(held.as_ptr() as usize);
        if used >= until {
            return used;
        }
        let deeper = stack_used(until, base);
        // Still needed after the call, so the call cannot reuse this frame.
        std::hint::black_box(&held);
        deeper
    }

    /// The run is on a thread with the first thread's name and far more
    /// stack than the first thread has, and the caller waits for it.
    #[cfg(unix)]
    #[test]
    fn a_run_has_the_stack_and_the_name_it_is_given() {
        const ME: &str = "memory::tests::a_run_has_the_stack_and_the_name_it_is_given";
        if child::is(ME) {
            on_a_deep_stack(|| {
                let base = 0u8;
                // Six times what the first thread has by default.
                let used = stack_used(48 << 20, &base as *const u8 as usize);
                let me = std::thread::current();
                println!("on {:?} with {} MiB used", me.name(), used >> 20);
            });
            println!("the run was waited for");
            return;
        }
        let out = child::ran(ME);
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{:?}\n{said}", out.status);
        let on = said.find("on Some(\"main\") with ").expect(&said);
        let mib: usize = said[on..]
            .split_whitespace()
            .nth(3)
            .and_then(|n| n.parse().ok())
            .expect(&said);
        assert!((48..STACK >> 20).contains(&mib), "{mib} MiB of stack used");
        let waited = said.find("the run was waited for").expect(&said);
        assert!(on < waited, "{said}");
    }

    /// A panic says which thread it was on and ends the run with 101. Both
    /// have to be what they were when the run was on the first thread.
    #[cfg(unix)]
    #[test]
    fn a_panic_on_the_deep_stack_ends_the_run_as_it_did() {
        const ME: &str = "memory::tests::a_panic_on_the_deep_stack_ends_the_run_as_it_did";
        if child::is(ME) {
            on_a_deep_stack(|| panic!("the run gave up"));
            println!("the run went on");
            return;
        }
        let out = child::ran(ME);
        let said = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(101), "{said}");
        let panic = said
            .lines()
            .position(|l| l.starts_with("thread 'main' ") && l.contains(" panicked at src/memory.rs:"))
            .expect(&said);
        assert_eq!(said.lines().nth(panic + 1), Some("the run gave up"), "{said}");
        assert!(!String::from_utf8_lossy(&out.stdout).contains("the run went on"));
    }

    /// More stack than there is address space for.
    #[cfg(unix)]
    #[test]
    fn a_stack_there_is_no_room_for_ends_the_run() {
        const ME: &str = "memory::tests::a_stack_there_is_no_room_for_ends_the_run";
        if child::is(ME) {
            on_a_stack_of(1 << 60, || println!("the run started"));
            println!("the run went on");
            return;
        }
        let said = child::aborted(ME);
        assert!(
            said.contains("memory allocation failed for the stack of the run"),
            "{said}"
        );
    }

    /// On glibc the run allocates from the heap the first thread uses, and
    /// not from one reserved for its own thread.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn the_run_allocates_where_the_first_thread_does() {
        const ME: &str = "memory::tests::the_run_allocates_where_the_first_thread_does";
        if child::is(ME) {
            on_a_deep_stack(|| {
                let small = Box::new([7u8; 64]);
                let at = small.as_ptr() as usize;
                let maps = std::fs::read_to_string("/proc/self/maps").expect("the maps");
                let heap = maps.lines().find(|l| l.ends_with("[heap]")).expect("a heap");
                let (from, to) = heap
                    .split_whitespace()
                    .next()
                    .and_then(|range| range.split_once('-'))
                    .expect("a range");
                let edge = |hex: &str| usize::from_str_radix(hex, 16).expect("an address");
                println!("in the first heap: {}", (edge(from)..edge(to)).contains(&at));
            });
            return;
        }
        let out = child::ran(ME);
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{:?}\n{said}", out.status);
        assert!(said.contains("in the first heap: true"), "{said}");
    }

    /// Any other answer is the caller's to read, as it was.
    #[test]
    fn a_directory_that_is_not_there_is_still_only_that() {
        let missing = read_dir("tests/fixtures/pkg/no-such-directory");
        assert_eq!(
            missing.err().map(|e| e.kind()),
            Some(std::io::ErrorKind::NotFound)
        );
        let a_file = read_dir("tests/fixtures/pkg/DESCRIPTION");
        assert!(a_file.is_err(), "a file is not a directory");
        let listed = read_dir("tests/fixtures/pkg").expect("the fixture package");
        assert!(listed.flatten().any(|e| e.file_name() == "DESCRIPTION"));
        assert_eq!(found(Ok(7)).ok(), Some(7));
    }

    /// What hides a memory failure stays out of everything but the tests,
    /// and only a run under a memory limit would show it otherwise. The
    /// libraries' own readers: one reads on past a decoder with no memory and
    /// the other cannot be made without it. And a directory listed without
    /// asking here.
    #[test]
    fn nothing_goes_round_this_module() {
        let mut files = 0;
        for entry in std::fs::read_dir("src").expect("list src").flatten() {
            // This file is the way through, and build_id.rs runs in the build.
            let name = entry.file_name();
            if name == "memory.rs" || name == "build_id.rs" {
                continue;
            }
            let text = std::fs::read_to_string(entry.path()).expect("read a source file");
            let code = text.split("\nmod tests {").next().unwrap_or_default();
            for hides in [
                "BzDecoder",
                "XzDecoder::new(",
                "XzDecoder::new_multi_decoder(",
                "fs::read_dir(",
            ] {
                assert!(
                    !code.contains(hides),
                    "{} uses {hides}",
                    entry.path().display()
                );
            }
            files += 1;
        }
        assert!(files >= 10, "the sources were read: {files}");
    }

    /// Every request goes to the system allocator as it was made.
    #[test]
    fn what_the_system_allocator_gives_is_handed_on() {
        for (size, align) in [(256, 8), (4096, 64)] {
            let layout = Layout::from_size_align(size, align).expect("a layout");
            unsafe {
                // Blocks given back full of 0xa5 are the likeliest to be
                // handed out again.
                let dirty: Vec<*mut u8> = (0..32).map(|_| EndsRun.alloc(layout)).collect();
                for &p in &dirty {
                    assert_eq!(p as usize % align, 0, "aligned as asked");
                    p.write_bytes(0xa5, size);
                }
                for &p in &dirty {
                    EndsRun.dealloc(p, layout);
                }
                let zeroed: Vec<*mut u8> =
                    (0..32).map(|_| EndsRun.alloc_zeroed(layout)).collect();
                for &z in &zeroed {
                    let held = std::slice::from_raw_parts(z, size);
                    assert!(held.iter().all(|b| *b == 0), "zeroed as asked");
                    z.write_bytes(0x5a, size);
                }
                for &z in &zeroed {
                    let big = EndsRun.realloc(z, layout, 1 << 20);
                    assert_eq!(big as usize % align, 0, "still aligned");
                    let held = std::slice::from_raw_parts(big, size);
                    assert!(held.iter().all(|b| *b == 0x5a), "what was held is still held");
                    EndsRun.dealloc(big, Layout::from_size_align(1 << 20, align).expect("a layout"));
                }
            }
        }
    }
}
