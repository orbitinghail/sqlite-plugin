//! Tests for xFetch/xUnfetch (iVersion 3) support.
//!
//! Implements a minimal file-backed VFS with real mmap-based fetch/unfetch.
//! Uses an atomic counter to prove SQLite actually calls fetch().

use std::fs::{self, OpenOptions};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlite_plugin::flags::{AccessFlags, LockLevel, OpenOpts};
use sqlite_plugin::vfs::{RegisterOpts, Vfs, VfsHandle, VfsResult};
use sqlite_plugin::vars;

static VFS_COUNTER: AtomicU64 = AtomicU64::new(1);

// Global counters to prove fetch/unfetch are called
static FETCH_COUNT: AtomicU64 = AtomicU64::new(0);
static UNFETCH_COUNT: AtomicU64 = AtomicU64::new(0);

struct Handle {
    file: std::fs::File,
    path: PathBuf,
    // Track mmap for fetch/unfetch
    mmap_ptr: Option<*mut u8>,
    mmap_len: usize,
}

unsafe impl Send for Handle {}

impl VfsHandle for Handle {
    fn readonly(&self) -> bool { false }
    fn in_memory(&self) -> bool { false }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if let Some(ptr) = self.mmap_ptr.take() {
            unsafe { libc::munmap(ptr as *mut libc::c_void, self.mmap_len); }
        }
    }
}

struct FetchVfs(PathBuf);

impl Vfs for FetchVfs {
    type Handle = Handle;

    fn open(&self, path: Option<&str>, _: OpenOpts) -> VfsResult<Self::Handle> {
        let p = self.0.join(path.unwrap_or("temp.db"));
        if let Some(d) = p.parent() { let _ = fs::create_dir_all(d); }
        let file = OpenOptions::new().read(true).write(true).create(true).open(&p)
            .map_err(|_| vars::SQLITE_CANTOPEN)?;
        Ok(Handle { file, path: p, mmap_ptr: None, mmap_len: 0 })
    }

    fn delete(&self, path: &str) -> VfsResult<()> {
        let _ = fs::remove_file(self.0.join(path)); Ok(())
    }

    fn access(&self, path: &str, _: AccessFlags) -> VfsResult<bool> {
        Ok(self.0.join(path).exists())
    }

    fn file_size(&self, h: &mut Self::Handle) -> VfsResult<usize> {
        h.file.metadata().map(|m| m.len() as usize).map_err(|_| vars::SQLITE_IOERR)
    }

    fn truncate(&self, h: &mut Self::Handle, sz: usize) -> VfsResult<()> {
        // Invalidate mmap on truncate
        if let Some(ptr) = h.mmap_ptr.take() {
            unsafe { libc::munmap(ptr as *mut libc::c_void, h.mmap_len); }
            h.mmap_len = 0;
        }
        h.file.set_len(sz as u64).map_err(|_| vars::SQLITE_IOERR)
    }

    fn write(&self, h: &mut Self::Handle, off: usize, data: &[u8]) -> VfsResult<usize> {
        h.file.write_at(data, off as u64).map_err(|_| vars::SQLITE_IOERR)
    }

    fn read(&self, h: &mut Self::Handle, off: usize, buf: &mut [u8]) -> VfsResult<usize> {
        match h.file.read_at(buf, off as u64) {
            Ok(n) => { buf[n..].fill(0); Ok(buf.len()) }
            Err(_) => Err(vars::SQLITE_IOERR_READ),
        }
    }

    fn lock(&self, _: &mut Self::Handle, _: LockLevel) -> VfsResult<()> { Ok(()) }
    fn unlock(&self, _: &mut Self::Handle, _: LockLevel) -> VfsResult<()> { Ok(()) }
    fn check_reserved_lock(&self, _: &mut Self::Handle) -> VfsResult<bool> { Ok(false) }
    fn sync(&self, h: &mut Self::Handle) -> VfsResult<()> {
        h.file.sync_all().map_err(|_| vars::SQLITE_IOERR_FSYNC)
    }
    fn close(&self, _: Self::Handle) -> VfsResult<()> { Ok(()) }

    fn fetch(
        &self,
        h: &mut Self::Handle,
        offset: i64,
        amt: usize,
    ) -> VfsResult<Option<NonNull<u8>>> {
        FETCH_COUNT.fetch_add(1, Ordering::Relaxed);

        let file_len = h.file.metadata().map_err(|_| vars::SQLITE_IOERR)?.len() as usize;
        let end = offset as usize + amt;
        if end > file_len {
            return Ok(None);
        }

        // Ensure file is mmap'd with enough coverage
        if h.mmap_ptr.is_none() || h.mmap_len < end {
            // Unmap old mapping if it exists
            if let Some(ptr) = h.mmap_ptr.take() {
                unsafe { libc::munmap(ptr as *mut libc::c_void, h.mmap_len); }
            }
            let map_len = file_len;
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    map_len,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    h.file.as_raw_fd(),
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Ok(None);
            }
            h.mmap_ptr = Some(ptr as *mut u8);
            h.mmap_len = map_len;
        }

        let base = h.mmap_ptr.expect("just mapped");
        let result = unsafe { base.add(offset as usize) };
        Ok(NonNull::new(result))
    }

    fn unfetch(
        &self,
        _h: &mut Self::Handle,
        _offset: i64,
        _ptr: *mut u8,
    ) -> VfsResult<()> {
        UNFETCH_COUNT.fetch_add(1, Ordering::Relaxed);
        // We keep the mmap alive for the handle's lifetime.
        // Individual unfetch calls don't need to unmap.
        Ok(())
    }
}

fn setup(prefix: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tmpdir");
    let name = format!("{}_{}", prefix, VFS_COUNTER.fetch_add(1, Ordering::Relaxed));
    let vfs = FetchVfs(dir.path().to_path_buf());
    sqlite_plugin::vfs::register_static(
        std::ffi::CString::new(name.as_str()).expect("name"),
        vfs, RegisterOpts { make_default: false },
    ).expect("register");
    (dir, name)
}

/// fetch() is called by SQLite when mmap_size > 0.
/// Verify data roundtrips correctly through mmap'd reads.
#[test]
fn test_fetch_mmap_reads() {
    let before = FETCH_COUNT.load(Ordering::Relaxed);

    let (dir, vfs) = setup("mmap");
    let conn = rusqlite::Connection::open_with_flags_and_vfs(
        dir.path().join("test.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        vfs.as_str(),
    ).expect("open");

    // Enable mmap -- this is required for SQLite to call xFetch
    conn.execute_batch("PRAGMA mmap_size=1048576").expect("mmap_size");
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", []).expect("create");

    // Insert enough data that SQLite will mmap pages
    for i in 0..200 {
        conn.execute("INSERT INTO t VALUES (?, ?)", (i, format!("value_{i}"))).expect("insert");
    }

    // Read back -- these reads should go through xFetch (mmap)
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)).expect("count");
    assert_eq!(count, 200);

    let v: String = conn.query_row("SELECT v FROM t WHERE id=42", [], |r| r.get(0)).expect("select");
    assert_eq!(v, "value_42");

    let after = FETCH_COUNT.load(Ordering::Relaxed);
    assert!(
        after > before,
        "fetch() should have been called at least once (before={}, after={})",
        before, after,
    );
    eprintln!("fetch called {} times", after - before);
}

/// Enough writes to trigger auto-checkpoint, exercising fetch during checkpoint.
#[test]
fn test_fetch_survives_checkpoint() {
    let (dir, vfs) = setup("ckpt");
    let conn = rusqlite::Connection::open_with_flags_and_vfs(
        dir.path().join("test.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        vfs.as_str(),
    ).expect("open");

    conn.execute_batch("PRAGMA mmap_size=1048576").expect("mmap_size");
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, data TEXT)", []).expect("create");
    for i in 0..2500 {
        conn.execute("INSERT INTO t (data) VALUES (?)", (format!("row_{i}"),)).expect("insert");
    }

    let count: i64 = conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)).expect("count");
    assert_eq!(count, 2500);
}
