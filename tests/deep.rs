// How deep an object the analyzer reads, and how a run ends when it cannot.
//
// The data reader calls itself once for every level of a nested object. On
// the first thread's 8 MiB that came to about 9,000 levels of a list and
// 40,000 arguments of a compiled call, a few more or fewer with every change
// to the reader, so a file near the edge was read by one build and aborted
// the next. The run has 64 MiB of stack of its own now.

mod common;
use common::*;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

// An unoptimized build spends about twenty times the stack on each level, so
// the tests that read deep are for the build that is released.

fn ints(v: &[i32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_be_bytes()).collect()
}

const NIL: i32 = 254;

/// A pairlist cell named `name`. Its value and the rest of the list follow.
fn tagged(name: &str) -> Vec<u8> {
    let mut out = ints(&[0x402, 1, 0x0004_0009, name.len() as i32]);
    out.extend_from_slice(name.as_bytes());
    out
}

fn symbol(name: &str) -> Vec<u8> {
    let mut out = ints(&[1, 0x0004_0009, name.len() as i32]);
    out.extend_from_slice(name.as_bytes());
    out
}

/// What `save(compress = FALSE, version = 2)` writes around the cells of an
/// image.
fn image(cells: &[u8]) -> Vec<u8> {
    let mut out = b"RDX2\nX\n".to_vec();
    out.extend(ints(&[2, 0x0004_0400, 0x0002_0300]));
    out.extend_from_slice(cells);
    out.extend(ints(&[NIL]));
    out
}

/// `x`, which is 1L inside `depth` lists of one element.
fn nested_list(depth: usize) -> Vec<u8> {
    let mut cells = tagged("x");
    cells.extend(ints(&[19, 1]).repeat(depth));
    cells.extend(ints(&[13, 1, 1]));
    image(&cells)
}

/// `n` objects, each one integer. The cells of an image are a list, and the
/// reader goes one call deeper for each.
fn many_objects(n: usize) -> Vec<u8> {
    let mut cells = Vec::new();
    for k in 1..=n {
        cells.extend(tagged(&format!("x{k:06}")));
        cells.extend(ints(&[13, 1, k as i32]));
    }
    image(&cells)
}

/// `x`, an empty integer vector whose attributes are another such vector,
/// `depth` of them. R does not write this, and the bytes allow it.
fn attribute_chain(depth: usize) -> Vec<u8> {
    let mut cells = tagged("x");
    cells.extend(ints(&[13 | 1 << 9, 0]).repeat(depth));
    cells.extend(ints(&[NIL]));
    image(&cells)
}

/// `x`, byte code whose one constant is `constant`.
fn byte_code(constant: &[u8]) -> Vec<u8> {
    let mut cells = tagged("x");
    // No shared constants, no code, one constant.
    cells.extend(ints(&[21, 0, 13, 0, 1]));
    cells.extend_from_slice(constant);
    image(&cells)
}

/// Byte code that holds the call `list(NULL, ...)` with `n` arguments, which
/// is read one call deeper for each argument.
fn many_arguments(n: usize) -> Vec<u8> {
    let mut call = ints(&[6, NIL, 0]);
    call.extend(symbol("list"));
    call.extend(ints(&[2, NIL, 0, NIL]).repeat(n));
    call.extend(ints(&[0, NIL]));
    byte_code(&call)
}

/// Byte code that holds a call whose function is a call, `depth` of them, and
/// at the bottom a string in Latin-1, which is turned into text there.
fn call_chain(depth: usize) -> Vec<u8> {
    let mut call = ints(&[6]);
    call.extend(ints(&[NIL, 6]).repeat(depth));
    call.extend(ints(&[NIL, 0, 9 | (1 << 2) << 12, 6]));
    call.extend_from_slice(b"cr\xe8me!");
    call.extend(ints(&[0, NIL]));
    call.extend(ints(&[0, NIL]).repeat(depth));
    byte_code(&call)
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(bytes).expect("gzip");
    e.finish().expect("gzip")
}

fn package(file: &str, bytes: &[u8]) -> Tree {
    tree_bytes(&[("DESCRIPTION", DESC.as_bytes()), (file, bytes)])
}

/// One run, with a statistics line asked for beside the package.
fn analyze(t: &Tree) -> (Output, bool) {
    let stats = t.root.with_extension("stats");
    let _ = std::fs::remove_file(&stats);
    let out = Command::new(env!("CARGO_BIN_EXE_rpkg-analyzer"))
        .args([t.path(), "--input-kind", "release"])
        .env_remove("RPKG_ANALYZER_CACHE_DIR")
        .env("RPKG_ANALYZER_STATS", &stats)
        .output()
        .expect("run the analyzer");
    let wrote = stats.exists();
    let _ = std::fs::remove_file(&stats);
    (out, wrote)
}

/// The dataset records of a run that has to finish.
fn datasets(what: &str, t: &Tree) -> Vec<Value> {
    let (out, stats) = analyze(t);
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{what}: {:?}\n{said}", out.status);
    assert!(out.stderr.is_empty(), "{what}: {said}");
    assert!(stats, "{what}: a run that finishes writes its statistics");
    String::from_utf8(out.stdout)
        .expect("utf-8 output")
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).expect("one JSON record per line"))
        .filter(|r| r["rec"] == "dataset")
        .collect()
}

/// Whether a run finished. One that did not has to have ended the way a
/// stack that ran out ends one: aborted, saying so, with no statistics.
#[cfg(unix)]
fn finished(what: &str, t: &Tree) -> bool {
    use std::os::unix::process::ExitStatusExt;
    let (out, stats) = analyze(t);
    let said = String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        assert!(out.stderr.is_empty(), "{what}: {said}");
        assert!(stats, "{what}: a run that finishes writes its statistics");
        return true;
    }
    assert_eq!(out.status.signal(), Some(6), "{what}: {:?}\n{said}", out.status);
    let overflow = said.lines().find(|l| l.starts_with("thread 'main' "));
    assert!(
        overflow.is_some_and(|l| l.ends_with(" has overflowed its stack")),
        "{what}: {said}"
    );
    assert!(!stats, "{what}: a run that was aborted writes no statistics");
    false
}

/// Twice what the first thread's stack held on any platform measured, and
/// each as a plain file and under gzip.
#[test]
#[cfg_attr(debug_assertions, ignore = "an unoptimized build does not read this deep")]
fn a_list_and_an_image_are_read_past_where_the_first_thread_stopped() {
    let levels = 20_000;
    for (how, bytes) in [("plain", nested_list(levels)), ("gzip", gzip(&nested_list(levels)))] {
        let found = datasets(how, &package("data/x.rda", &bytes));
        assert_eq!(found.len(), 1, "{how}");
        assert_eq!(found[0]["name"], "x", "{how}");
        // The record follows a list twelve levels down and no further, so
        // what it shows of the depth is that the file was read.
        assert_eq!(found[0]["kind"], "list", "{how}");
        assert_eq!(found[0]["confidence"], "exact", "{how}");
    }
    for (how, bytes) in [("plain", many_objects(levels)), ("gzip", gzip(&many_objects(levels)))] {
        let found = datasets(how, &package("data/img.RData", &bytes));
        assert_eq!(found.len(), levels, "{how}");
        assert_eq!(found[0]["name"], "x000001", "{how}");
        assert_eq!(found[levels - 1]["name"], format!("x{levels:06}"), "{how}");
        assert_eq!(found[levels - 1]["col_max"], levels, "{how}");
    }
    let found = datasets("attributes", &package("data/x.rda", &attribute_chain(levels)));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["type"], "integer");
}

/// Byte code is read by calls of its own, smaller ones, which the first
/// thread's stack held about 40,000 of.
#[test]
#[cfg_attr(debug_assertions, ignore = "an unoptimized build does not read this deep")]
fn byte_code_is_read_past_where_the_first_thread_stopped() {
    for (what, bytes) in [
        ("100,000 arguments", many_arguments(100_000)),
        ("100,000 arguments under gzip", gzip(&many_arguments(100_000))),
        // The depth at which one build read and the next did not, both ways.
        ("40,077 calls", call_chain(40_077)),
        ("100,000 calls", call_chain(100_000)),
    ] {
        let found = datasets(what, &package("data/x.rda", &bytes));
        assert_eq!(found.len(), 1, "{what}");
        assert_eq!(found[0]["name"], "x", "{what}");
        assert_eq!(found[0]["kind"], "object", "{what}");
    }
}

/// Files R wrote, at depths the first thread's stack could just hold or just
/// not, depending on the build: tests/fixtures/make.R says how each is made.
#[test]
#[cfg_attr(debug_assertions, ignore = "an unoptimized build does not read this deep")]
fn what_r_wrote_near_the_old_limit_is_read() {
    let fixture = |name: &str| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/deep").join(name);
        package(&format!("data/{name}"), &std::fs::read(path).expect("a fixture"))
    };
    let found = datasets("nested_list", &fixture("nested_list.rda"));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["name"], "nested_list");
    assert_eq!(found[0]["kind"], "list");
    assert_eq!(found[0]["confidence"], "exact");
    assert_eq!(found[0]["compression"], "gzip");

    let found = datasets("many_objects", &fixture("many_objects.rda"));
    assert_eq!(found.len(), 6000);
    assert_eq!(found[0]["name"], "x0001");
    assert_eq!(found[5999]["name"], "x6000");
    assert_eq!(found[5999]["col_min"], 6000);

    let found = datasets("many_nulls", &fixture("many_nulls.rda"));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["name"], "many_nulls");
    assert_eq!(found[0]["kind"], "object");
}

/// Far past any stack, the run still ends as a run out of stack always has.
/// A thread that is not the first has one guard page under its stack, and
/// that is enough for a reader whose calls are smaller than a page.
#[cfg(unix)]
#[test]
fn a_file_nested_past_any_stack_aborts_the_run_and_says_why() {
    for (what, file, bytes) in [
        ("a list", "data/x.rda", nested_list(600_000)),
        ("a list under gzip", "data/x.rda", gzip(&nested_list(600_000))),
        ("attributes", "data/x.rda", attribute_chain(600_000)),
        ("arguments", "data/x.rda", many_arguments(1_000_000)),
        ("calls", "data/x.rda", call_chain(1_000_000)),
    ] {
        assert!(!finished(what, &package(file, &bytes)), "{what} was read");
    }
}

/// A damaged file is not followed down the stack. Once its parse has used
/// 1 MiB the reader asks the decoder whether the file is sound, and that is
/// measured from where the read began, on whichever thread it began. The
/// same bytes ran the run out of stack just above, where the file was whole.
#[test]
fn a_damaged_file_nested_past_any_stack_is_only_a_damaged_file() {
    let mut cut = gzip(&nested_list(600_000));
    cut.truncate(cut.len() - 2);
    let found = datasets("cut short", &package("data/x.rda", &cut));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["confidence"], "degraded");
    assert_eq!(found[0]["notes"], "unexpected end of file");
}

/// Between the two, each depth is read or ends the run in that one way, and
/// nothing deeper than a depth that was not read is read.
#[cfg(unix)]
#[test]
fn every_depth_is_read_or_ends_the_run_cleanly() {
    let mut read = Vec::new();
    for levels in [1_000, 3_000, 50_000, 100_000, 200_000] {
        read.push(finished(&format!("{levels} levels"), &package("data/x.rda", &nested_list(levels))));
    }
    assert!(read.is_sorted_by(|shallow, deep| shallow >= deep), "{read:?}");
    assert!(read[0], "a thousand levels are read by any build");

    let mut read = Vec::new();
    for n in [10_000, 200_000, 300_000, 400_000] {
        read.push(finished(&format!("{n} arguments"), &package("data/x.rda", &many_arguments(n))));
    }
    assert!(read.is_sorted_by(|shallow, deep| shallow >= deep), "{read:?}");
    assert!(read[0], "ten thousand arguments are read by any build");
}

/// A panic names the first thread, as it did when the run was on it, and
/// ends the run with 101 and no statistics line.
#[test]
fn a_panic_reads_and_exits_as_it_did_on_the_first_thread() {
    let t = tree(&[("keep", "")]);
    let stats = t.root.join("stats.ndjson");
    let missing = t.root.join("no-such-file.R");
    let out = Command::new(env!("CARGO_BIN_EXE_rpkg-analyzer"))
        .args(["--sexp", missing.to_str().expect("utf-8 temp path")])
        .env("RPKG_ANALYZER_STATS", &stats)
        .output()
        .expect("run the analyzer");
    let said = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(101), "{said}");
    assert!(out.stdout.is_empty());
    let panic = said.lines().find(|l| l.starts_with("thread 'main' ")).expect("a panic message");
    assert!(panic.contains(" panicked at src/main.rs:"), "{said}");
    assert!(said.contains("\nread: "), "{said}");
    assert!(!stats.exists());
}

/// Records are printed as they are made, so a stdout that takes no more of
/// them is a panic in the middle of a run.
///
/// The stdout is a socket shut for writing, which refuses a write whoever
/// holds its other end, and the other end stays open here. A pipe whose
/// reader was closed refuses only while no process holds that end, and where
/// a pipe is made in one step and kept from new processes in a second, a
/// process another test starts between the two holds it.
#[cfg(unix)]
#[test]
fn a_panic_in_the_middle_of_a_run_ends_it_the_same_way() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    let t = package("R/a.R", b"f <- function(x) x\n");
    let stats = t.root.with_extension("stats");
    let (_open, into) = UnixStream::pair().expect("a pair of sockets");
    into.shutdown(std::net::Shutdown::Write).expect("shut it for writing");
    let out = Command::new(env!("CARGO_BIN_EXE_rpkg-analyzer"))
        .args([t.path(), "--input-kind", "release"])
        .env("RPKG_ANALYZER_STATS", &stats)
        .stdout(OwnedFd::from(into))
        .output()
        .expect("run the analyzer");
    let said = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(101), "{said}");
    let panic = said.lines().find(|l| l.starts_with("thread 'main' ")).expect("a panic message");
    assert!(panic.contains(" panicked at "), "{said}");
    assert!(said.contains("failed printing to stdout"), "{said}");
    assert!(!stats.exists());
}

/// The usage line and the version come from the same thread and leave with
/// the statuses they had.
#[test]
fn the_other_ways_out_are_the_ones_they_were() {
    let usage = run(&[]);
    assert_eq!(usage.status.code(), Some(2));
    assert!(usage.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&usage.stderr),
        "usage: rpkg-analyzer <package_dir> --input-kind release|git\n"
    );
    let version = run(&["--version"]);
    assert_eq!(version.status.code(), Some(0));
    assert!(version.stderr.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        format!("rpkg-analyzer {}\n", env!("CARGO_PKG_VERSION"))
    );
}

/// Under an address-space limit too small for the stack the run does not
/// start: nothing is printed, no statistics line is written, and it is
/// aborted like any run that could not get its memory. 48 MiB is enough to
/// load the program and not enough for 64 MiB of stack.
#[cfg(target_os = "linux")]
#[test]
fn a_run_with_no_room_for_its_stack_prints_nothing() {
    use std::os::unix::process::ExitStatusExt;
    let t = package("R/a.R", b"f <- function(x) x\n");
    let stats = t.root.with_extension("stats");
    let under = |kib: usize, args: &[&str]| {
        Command::new("sh")
            .arg("-c")
            .arg(format!("ulimit -v {kib}; [ \"$(ulimit -v)\" = {kib} ] || exit 77; exec \"$@\""))
            .arg("sh")
            .arg(env!("CARGO_BIN_EXE_rpkg-analyzer"))
            .args(args)
            .env("RPKG_ANALYZER_STATS", &stats)
            .output()
            .expect("run the analyzer under a limit")
    };
    for args in [&[t.path(), "--input-kind", "release"][..], &["--version"][..]] {
        let out = under(48 * 1024, args);
        if out.status.code() == Some(77) {
            // An emulated system may take the limit and not apply it.
            eprintln!("no address-space limit can be set here");
            return;
        }
        let said = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.signal(), Some(6), "{:?}\n{said}", out.status);
        assert!(out.stdout.is_empty());
        assert!(said.starts_with("memory allocation failed for the stack of the run: "), "{said}");
        assert_eq!(said.lines().count(), 1, "{said}");
        assert!(!stats.exists());
    }
    // With room for it the same run finishes.
    let out = under(512 * 1024, &[t.path(), "--input-kind", "release"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(stats.exists());
    let _ = std::fs::remove_file(&stats);
}
