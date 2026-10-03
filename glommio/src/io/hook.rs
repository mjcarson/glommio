// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
// This product includes software developed at Datadog (https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//
//! A hook every file operation asks before it runs, so a test can fail a directory on purpose
//!
//! A program that wants to see what it does when a disk tears a write, fills, or stops
//! answering sets one [`IoHook`] for the process with [`set_io_hook`]. From then on every open,
//! read, write, sync and metadata operation on a [`DmaFile`](super::DmaFile), a
//! [`BufferedFile`](super::BufferedFile) or a [`Directory`](super::Directory), and every
//! [`rename`](super::rename) and [`remove`](super::remove), asks it first with the path of the
//! file it is about, and does what it is told: go ahead, fail with an error, or write only the
//! start of a write.
//!
//! Until a hook is set the cost is one atomic load per operation and no path is looked at, so a
//! program that never sets one pays nothing else for it. The hook is set once and never
//! replaced; what it decides is the hook's own state to change.

use std::{io, path::Path, sync::OnceLock};

/// What a file operation is about to do
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoOp {
    /// Open or create a file or a directory
    Open,
    /// Read some bytes of a file
    Read {
        /// Where the read starts
        pos: u64,
        /// How many bytes it asks for
        len: usize,
    },
    /// Write some bytes of a file
    Write {
        /// Where the write starts
        pos: u64,
        /// How many bytes it writes
        len: usize,
    },
    /// Flush a file's or a directory's writes to the device
    Sync,
    /// Rename, remove, truncate or allocate: a change to what names or holds the bytes, rather
    /// than to the bytes
    Meta,
}

/// What the hook decided about an operation
#[derive(Debug)]
pub enum IoVerdict {
    /// Go ahead as if there were no hook
    Proceed,
    /// Fail with this error, without doing anything
    Fail(io::Error),
    /// Write only the first this many bytes of a write, make them durable, and then tell the
    /// hook through [`IoHook::torn`]
    ///
    /// A file opened for direct I/O writes whole blocks, so the prefix is rounded down to the
    /// file's alignment. Answered to anything but a write, it is taken as [`IoVerdict::Proceed`].
    Tear(usize),
}

/// What decides whether a file operation runs
pub trait IoHook: Send + Sync {
    /// Decide an operation on a file before it runs
    ///
    /// # Arguments
    ///
    /// * `path` - The path of the file or directory the operation is on
    /// * `op` - What the operation is about to do
    fn check(&self, path: &Path, op: IoOp) -> IoVerdict;

    /// Hear that a torn write's prefix is on the device
    ///
    /// Called once the prefix is written and synced. What a torn write models is a crash in the
    /// middle of it, so a hook ends the process here; if it returns, the write fails with an
    /// I/O error instead.
    ///
    /// # Arguments
    ///
    /// * `path` - The path of the file the write was torn in
    /// * `written` - How many bytes of the write reached the device
    fn torn(&self, path: &Path, written: usize);
}

/// The hook every file operation in this process asks, once one is set
static HOOK: OnceLock<&'static dyn IoHook> = OnceLock::new();

/// Set the hook every file operation in this process asks from now on
///
/// The hook is set once and never replaced. Returns whether this call set it; a second call
/// leaves the first hook in place.
///
/// # Arguments
///
/// * `hook` - The hook
pub fn set_io_hook(hook: &'static dyn IoHook) -> bool {
    HOOK.set(hook).is_ok()
}

/// Whether a hook is set, which is all an operation looks at when none is
#[inline]
pub(crate) fn is_set() -> bool {
    HOOK.get().is_some()
}

/// Ask the hook about an operation, if one is set and the file has a path to ask about
///
/// # Arguments
///
/// * `path` - The path of the file, if it has one
/// * `op` - What the operation is about to do
pub(crate) fn check(path: Option<&Path>, op: IoOp) -> IoVerdict {
    match (HOOK.get(), path) {
        (Some(hook), Some(path)) => hook.check(path, op),
        _ => IoVerdict::Proceed,
    }
}

/// Suspend once, as an operation does before its completion arrives
///
/// An answer from the hook is ready at once, where a real operation's error comes back from the
/// ring after the task has suspended. A caller may rely on that - a stream writer's flush task
/// takes the writer's state once its write returns, which is safe only because the write has
/// suspended first - so every refusal suspends once before it is returned.
pub(crate) async fn settle() {
    futures_lite::future::yield_now().await;
}

/// Tell the hook a torn write's prefix is on the device, and fail the write if it returns
///
/// # Arguments
///
/// * `path` - The path of the file the write was torn in
/// * `written` - How many bytes of the write reached the device
pub(crate) fn torn(path: &Path, written: usize) -> io::Error {
    if let Some(hook) = HOOK.get() {
        hook.torn(path, written);
    }
    io::Error::other(format!(
        "a write to {} was torn after {written} bytes",
        path.display()
    ))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        io::{BufferedFile, DmaFile},
        test_utils::make_tmp_test_directory,
        LocalExecutor,
    };
    use std::{
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
    };

    /// A hook for one directory: lose it, or tear its next write at some bytes
    struct TestHook {
        /// The directory the hook acts on, and nothing outside it
        dir: Mutex<Option<PathBuf>>,
        /// Whether every operation in it fails
        lost: Mutex<bool>,
        /// Where the next write is torn, if it is
        tear: Mutex<Option<usize>>,
        /// How many bytes the last torn write left
        torn: AtomicUsize,
    }

    impl IoHook for TestHook {
        fn check(&self, path: &Path, op: IoOp) -> IoVerdict {
            // only the directory under test, component by component
            let inside = self
                .dir
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|dir| path.starts_with(dir));
            if !inside {
                return IoVerdict::Proceed;
            }
            if *self.lost.lock().unwrap() {
                return IoVerdict::Fail(io::Error::from_raw_os_error(libc::EIO));
            }
            // a tear waiting is taken by the next write in the directory and nothing else
            let mut tear = self.tear.lock().unwrap();
            match (op, *tear) {
                (IoOp::Write { .. }, Some(at)) => {
                    *tear = None;
                    IoVerdict::Tear(at)
                }
                _ => IoVerdict::Proceed,
            }
        }

        fn torn(&self, _path: &Path, written: usize) {
            // a test hears of it rather than ending the process
            self.torn.store(written, Ordering::SeqCst);
        }
    }

    /// The one hook this test binary sets
    static TEST_HOOK: TestHook = TestHook {
        dir: Mutex::new(None),
        lost: Mutex::new(false),
        tear: Mutex::new(None),
        torn: AtomicUsize::new(usize::MAX),
    };

    #[test]
    fn a_hook_fails_and_tears_only_its_directory() {
        set_io_hook(&TEST_HOOK);
        let ex = LocalExecutor::default();
        ex.run(async {
            let dir = make_tmp_test_directory("io_hook");
            let inside = dir.path.join("inside");
            let outside = dir.path.join("outside");
            std::fs::create_dir_all(&inside).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            *TEST_HOOK.dir.lock().unwrap() = Some(inside.clone());

            // a buffered write torn at ten bytes leaves ten bytes and fails
            let file = BufferedFile::create(inside.join("buffered")).await.unwrap();
            *TEST_HOOK.tear.lock().unwrap() = Some(10);
            assert!(file.write_at(vec![7u8; 100], 0).await.is_err());
            assert_eq!(TEST_HOOK.torn.load(Ordering::SeqCst), 10);
            assert_eq!(
                std::fs::read(inside.join("buffered")).unwrap(),
                vec![7u8; 10]
            );
            file.close().await.unwrap();

            // a direct write torn past one block keeps whole blocks only
            let file = DmaFile::create(inside.join("direct")).await.unwrap();
            let align = file.alignment() as usize;
            let mut buf = file.alloc_dma_buffer(align * 4);
            buf.as_bytes_mut().fill(9);
            *TEST_HOOK.tear.lock().unwrap() = Some(align + 3);
            assert!(file.write_at(buf, 0).await.is_err());
            assert_eq!(TEST_HOOK.torn.load(Ordering::SeqCst), align);
            assert_eq!(
                std::fs::read(inside.join("direct")).unwrap(),
                vec![9u8; align]
            );
            file.close().await.unwrap();

            // a lost directory fails every operation in it, and nothing beside it
            *TEST_HOOK.lost.lock().unwrap() = true;
            assert!(BufferedFile::create(inside.join("again")).await.is_err());
            assert!(crate::io::remove(inside.join("buffered")).await.is_err());
            let beside = BufferedFile::create(outside.join("fine")).await.unwrap();
            beside.write_at(vec![1u8; 4], 0).await.unwrap();
            beside.fdatasync().await.unwrap();
            beside.close().await.unwrap();
            *TEST_HOOK.lost.lock().unwrap() = false;
            *TEST_HOOK.dir.lock().unwrap() = None;
        });
    }
}
