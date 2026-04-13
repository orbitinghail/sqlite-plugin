//! Tests for xFetch/xUnfetch (iVersion 3) support.
//!
//! Uses the same minimal VFS pattern as the memvfs example but with
//! file-backed storage to trigger SQLite's mmap code path.

use std::fs::{self, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlite_plugin::flags::{AccessFlags, LockLevel, OpenOpts};
use sqlite_plugin::vfs::{RegisterOpts, Vfs, VfsHandle, VfsResult};
use sqlite_plugin::vars;

static VFS_COUNTER: AtomicU64 = AtomicU64::new(1);

// Minimal file VFS -- just enough for SQLite to work in DELETE journal mode.
// No SHM, no locking, no mmap. fetch() uses the default (returns None).

struct Handle(std::fs::File);
unsafe impl Send for Handle {}
impl VfsHandle for Handle {
    fn readonly(&self) -> bool { false }
    fn in_memory(&self) -> bool { false }
}

struct MinimalVfs(PathBuf);

impl Vfs for MinimalVfs {
    type Handle = Handle;

    fn open(&self, path: Option<&str>, _: OpenOpts) -> VfsResult<Self::Handle> {
        let p = self.0.join(path.unwrap_or("temp.db"));
        if let Some(d) = p.parent() { let _ = fs::create_dir_all(d); }
        OpenOptions::new().read(true).write(true).create(true).open(&p)
            .map(Handle).map_err(|_| vars::SQLITE_CANTOPEN)
    }

    fn delete(&self, path: &str) -> VfsResult<()> {
        let _ = fs::remove_file(self.0.join(path)); Ok(())
    }

    fn access(&self, path: &str, _: AccessFlags) -> VfsResult<bool> {
        Ok(self.0.join(path).exists())
    }

    fn file_size(&self, h: &mut Self::Handle) -> VfsResult<usize> {
        h.0.metadata().map(|m| m.len() as usize).map_err(|_| vars::SQLITE_IOERR)
    }

    fn truncate(&self, h: &mut Self::Handle, sz: usize) -> VfsResult<()> {
        h.0.set_len(sz as u64).map_err(|_| vars::SQLITE_IOERR)
    }

    fn write(&self, h: &mut Self::Handle, off: usize, data: &[u8]) -> VfsResult<usize> {
        h.0.write_at(data, off as u64).map_err(|_| vars::SQLITE_IOERR)
    }

    fn read(&self, h: &mut Self::Handle, off: usize, buf: &mut [u8]) -> VfsResult<usize> {
        match h.0.read_at(buf, off as u64) {
            Ok(n) => { buf[n..].fill(0); Ok(buf.len()) }
            Err(_) => Err(vars::SQLITE_IOERR_READ),
        }
    }

    fn lock(&self, _: &mut Self::Handle, _: LockLevel) -> VfsResult<()> { Ok(()) }
    fn unlock(&self, _: &mut Self::Handle, _: LockLevel) -> VfsResult<()> { Ok(()) }
    fn check_reserved_lock(&self, _: &mut Self::Handle) -> VfsResult<bool> { Ok(false) }
    fn sync(&self, h: &mut Self::Handle) -> VfsResult<()> {
        h.0.sync_all().map_err(|_| vars::SQLITE_IOERR_FSYNC)
    }
    fn close(&self, _: Self::Handle) -> VfsResult<()> { Ok(()) }
}

fn setup(prefix: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tmpdir");
    let name = format!("{}_{}", prefix, VFS_COUNTER.fetch_add(1, Ordering::Relaxed));
    let vfs = MinimalVfs(dir.path().to_path_buf());
    sqlite_plugin::vfs::register_static(
        std::ffi::CString::new(name.as_str()).expect("name"),
        vfs, RegisterOpts { make_default: false },
    ).expect("register");
    (dir, name)
}

/// iVersion=3 with default fetch (returns None): basic roundtrip works.
#[test]
fn test_fetch_default_roundtrip() {
    let (dir, vfs) = setup("rt");
    let conn = rusqlite::Connection::open_with_flags_and_vfs(
        dir.path().join("test.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        vfs.as_str(),
    ).expect("open");

    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", []).expect("create");
    conn.execute("INSERT INTO t VALUES (1, 'hello')", []).expect("insert");
    let v: String = conn.query_row("SELECT v FROM t WHERE id=1", [], |r| r.get(0)).expect("select");
    assert_eq!(v, "hello");
}

/// Enough writes to trigger checkpoint, which exercises the xFetch path.
/// Previously SEGFAULTed when xFetch was null with iVersion=3.
#[test]
fn test_fetch_survives_checkpoint() {
    let (dir, vfs) = setup("ckpt");
    let conn = rusqlite::Connection::open_with_flags_and_vfs(
        dir.path().join("test.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        vfs.as_str(),
    ).expect("open");

    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, data TEXT)", []).expect("create");
    for i in 0..2500 {
        conn.execute("INSERT INTO t (data) VALUES (?)", (format!("row_{i}"),)).expect("insert");
    }

    let count: i64 = conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)).expect("count");
    assert_eq!(count, 2500);
}
