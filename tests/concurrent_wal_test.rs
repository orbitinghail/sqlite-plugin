//! Regression test for iVersion=3 + xFetch=None SEGFAULT.
//!
//! Uses a trivial file-backed VFS (no external dependencies) to prove
//! the bug is in sqlite-plugin's io_methods setup, not in any downstream
//! VFS implementation.
//!
//! The bug: iVersion was set to 3 (declaring xFetch/xUnfetch support) but
//! both function pointers were None. SQLite called xFetch during WAL
//! checkpoint page reads under concurrent load, jumping to address 0x0.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use sqlite_plugin::flags::{AccessFlags, LockLevel, OpenOpts, ShmLockMode};
use sqlite_plugin::vfs::{RegisterOpts, Vfs, VfsHandle, VfsResult};
use sqlite_plugin::vars;

// ── Minimal file-backed VFS ────────────────────────────────────────

static CONN_ID: AtomicU64 = AtomicU64::new(1);

struct SimpleHandle {
    file: std::fs::File,
    path: PathBuf,
    shm_regions: HashMap<u32, *mut u8>,
    shm_file: Option<std::fs::File>,
}

unsafe impl Send for SimpleHandle {}

const SHM_REGION_SIZE: usize = 32768;

impl VfsHandle for SimpleHandle {
    fn readonly(&self) -> bool {
        false
    }
    fn in_memory(&self) -> bool {
        false
    }
}

impl Drop for SimpleHandle {
    fn drop(&mut self) {
        for (_, ptr) in self.shm_regions.drain() {
            unsafe { libc::munmap(ptr as *mut libc::c_void, SHM_REGION_SIZE); }
        }
    }
}

struct SimpleVfs {
    base_dir: PathBuf,
}

impl Vfs for SimpleVfs {
    type Handle = SimpleHandle;

    fn open(&self, path: Option<&str>, _opts: OpenOpts) -> VfsResult<Self::Handle> {
        let name = path.unwrap_or("temp.db");
        let full_path = self.base_dir.join(name);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).map_err(|_| vars::SQLITE_CANTOPEN)?;
        }
        let file = OpenOptions::new()
            .read(true).write(true).create(true)
            .open(&full_path)
            .map_err(|_| vars::SQLITE_CANTOPEN)?;
        Ok(SimpleHandle { file, path: full_path, shm_regions: HashMap::new(), shm_file: None })
    }

    fn delete(&self, path: &str) -> VfsResult<()> {
        let _ = fs::remove_file(self.base_dir.join(path));
        Ok(())
    }

    fn access(&self, path: &str, _flags: AccessFlags) -> VfsResult<bool> {
        Ok(self.base_dir.join(path).exists())
    }

    fn file_size(&self, handle: &mut Self::Handle) -> VfsResult<usize> {
        handle.file.metadata().map(|m| m.len() as usize).map_err(|_| vars::SQLITE_IOERR)
    }

    fn truncate(&self, handle: &mut Self::Handle, size: usize) -> VfsResult<()> {
        handle.file.set_len(size as u64).map_err(|_| vars::SQLITE_IOERR)
    }

    fn write(&self, handle: &mut Self::Handle, offset: usize, data: &[u8]) -> VfsResult<usize> {
        handle.file.write_at(data, offset as u64).map_err(|_| vars::SQLITE_IOERR)
    }

    fn read(&self, handle: &mut Self::Handle, offset: usize, buf: &mut [u8]) -> VfsResult<usize> {
        match handle.file.read_at(buf, offset as u64) {
            Ok(n) => { buf[n..].fill(0); Ok(buf.len()) }
            Err(_) => Err(vars::SQLITE_IOERR_READ),
        }
    }

    fn lock(&self, _handle: &mut Self::Handle, _level: LockLevel) -> VfsResult<()> { Ok(()) }
    fn unlock(&self, _handle: &mut Self::Handle, _level: LockLevel) -> VfsResult<()> { Ok(()) }
    fn check_reserved_lock(&self, _handle: &mut Self::Handle) -> VfsResult<bool> { Ok(false) }

    fn sync(&self, handle: &mut Self::Handle) -> VfsResult<()> {
        handle.file.sync_all().map_err(|_| vars::SQLITE_IOERR_FSYNC)
    }

    fn close(&self, _handle: Self::Handle) -> VfsResult<()> { Ok(()) }

    fn shm_map(
        &self, handle: &mut Self::Handle, region_idx: usize, _region_size: usize, _extend: bool,
    ) -> VfsResult<Option<NonNull<u8>>> {
        let region = region_idx as u32;
        if let Some(&ptr) = handle.shm_regions.get(&region) {
            return Ok(NonNull::new(ptr));
        }
        use std::os::unix::io::AsRawFd;
        let offset = region as usize * SHM_REGION_SIZE;
        if handle.shm_file.is_none() {
            let shm_path = handle.path.with_extension("db-shm");
            handle.shm_file = Some(OpenOptions::new().read(true).write(true).create(true)
                .open(&shm_path).map_err(|_| vars::SQLITE_IOERR)?);
        }
        let file = handle.shm_file.as_ref().expect("just set");
        let file_len = file.metadata().map_err(|_| vars::SQLITE_IOERR)?.len() as usize;
        if file_len < offset + SHM_REGION_SIZE {
            file.set_len((offset + SHM_REGION_SIZE) as u64).map_err(|_| vars::SQLITE_IOERR)?;
        }
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), SHM_REGION_SIZE,
                libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED,
                file.as_raw_fd(), offset as libc::off_t)
        };
        if ptr == libc::MAP_FAILED { return Err(vars::SQLITE_IOERR); }
        let ptr = ptr as *mut u8;
        handle.shm_regions.insert(region, ptr);
        Ok(NonNull::new(ptr))
    }

    fn shm_lock(&self, _handle: &mut Self::Handle, _offset: u32, _count: u32, _mode: ShmLockMode) -> VfsResult<()> {
        Ok(()) // no-op locking (single writer test only)
    }

    fn shm_barrier(&self, _handle: &mut Self::Handle) {
        std::sync::atomic::fence(Ordering::SeqCst);
    }

    fn shm_unmap(&self, handle: &mut Self::Handle, delete: bool) -> VfsResult<()> {
        for (_, ptr) in handle.shm_regions.drain() {
            unsafe { libc::munmap(ptr as *mut libc::c_void, SHM_REGION_SIZE); }
        }
        if delete {
            let _ = fs::remove_file(handle.path.with_extension("db-shm"));
        }
        Ok(())
    }
}

// ── Regression test ────────────────────────────────────────────────

/// Concurrent WAL: 1 writer + 4 readers. With the iVersion=3 bug,
/// this SEGFAULTs ~60% of the time because SQLite calls the null
/// xFetch function pointer during WAL checkpoint page reads.
///
/// Single writer avoids WAL-index corruption from the no-op shm_lock.
#[test]
fn test_concurrent_wal_no_segfault() {
    let tmpdir = tempfile::tempdir().expect("tmpdir");
    let vfs_name = format!("simple_vfs_{}", CONN_ID.fetch_add(1, Ordering::Relaxed));

    let vfs = SimpleVfs { base_dir: tmpdir.path().to_path_buf() };
    sqlite_plugin::vfs::register_static(
        std::ffi::CString::new(vfs_name.as_str()).expect("vfs name"),
        vfs,
        RegisterOpts { make_default: false },
    ).expect("register VFS");

    // Setup
    {
        let conn = rusqlite::Connection::open_with_flags_and_vfs(
            tmpdir.path().join("test.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
            vfs_name.as_str(),
        ).expect("open");
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").expect("WAL");
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, data TEXT)", []).expect("create");
        conn.execute("BEGIN", []).expect("begin");
        for i in 0..1000 {
            conn.execute("INSERT INTO t (data) VALUES (?)", (format!("row_{}", i),)).expect("insert");
        }
        conn.execute("COMMIT", []).expect("commit");
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").expect("checkpoint");
    }

    let stop = Arc::new(AtomicBool::new(false));
    let read_count = Arc::new(AtomicUsize::new(0));
    let write_count = Arc::new(AtomicUsize::new(0));
    let db_dir = tmpdir.path().to_path_buf();
    let mut handles = Vec::new();

    // 4 readers
    for _ in 0..4 {
        let stop = Arc::clone(&stop);
        let reads = Arc::clone(&read_count);
        let dir = db_dir.clone();
        let vn = vfs_name.clone();
        handles.push(thread::spawn(move || {
            let conn = rusqlite::Connection::open_with_flags_and_vfs(
                dir.join("test.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY, vn.as_str(),
            ).expect("open reader");
            let mut i = 0usize;
            while !stop.load(Ordering::Relaxed) {
                // Errors from no-op locking are expected; the test is that we don't SEGFAULT
                if conn.query_row("SELECT data FROM t WHERE id = ?",
                    [((i % 1000) + 1) as i64], |r| r.get::<_, String>(0)).is_ok() {
                    reads.fetch_add(1, Ordering::Relaxed);
                }
                i += 1;
            }
        }));
    }

    // 1 writer (single writer avoids WAL-index corruption from no-op locks)
    {
        let stop = Arc::clone(&stop);
        let writes = Arc::clone(&write_count);
        let dir = db_dir.clone();
        let vn = vfs_name.clone();
        handles.push(thread::spawn(move || {
            let conn = rusqlite::Connection::open_with_flags_and_vfs(
                dir.join("test.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE, vn.as_str(),
            ).expect("open writer");
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").expect("WAL");
            let mut i = 0usize;
            while !stop.load(Ordering::Relaxed) {
                if conn.execute("INSERT INTO t (data) VALUES (?)", (format!("w_{}", i),)).is_ok() {
                    writes.fetch_add(1, Ordering::Relaxed);
                }
                i += 1;
            }
        }));
    }

    thread::sleep(Duration::from_secs(3));
    stop.store(true, Ordering::Relaxed);
    for h in handles { h.join().expect("thread join"); }

    let reads = read_count.load(Ordering::Relaxed);
    let writes = write_count.load(Ordering::Relaxed);
    assert!(reads > 0, "should have completed some reads");
    assert!(writes > 0, "should have completed some writes");
    eprintln!("concurrent WAL test: {} reads, {} writes (no SEGFAULT)", reads, writes);
}
