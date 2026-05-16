//! Lightweight tests for VFS C-API contract details enforced by the wrapper:
//! - PR #83: x_open must set sqlite3_file.pMethods (to NULL on failure).
//! - PR #84: x_read must zero-fill the tail and return SQLITE_IOERR_SHORT_READ
//!   when the underlying Vfs::read reports fewer bytes than requested.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::ffi;
use sqlite_plugin::flags::{AccessFlags, LockLevel, OpenOpts};
use sqlite_plugin::vars;
use sqlite_plugin::vfs::{RegisterOpts, Vfs, VfsHandle, VfsResult};

static VFS_COUNTER: AtomicU64 = AtomicU64::new(1);

fn unique_name(prefix: &str) -> CString {
    let n = VFS_COUNTER.fetch_add(1, Ordering::Relaxed);
    CString::new(format!("{prefix}_{n}")).expect("vfs name")
}

// 64 bytes, 8-aligned — large enough for FileWrapper<ZST handle> on any platform.
#[repr(C, align(8))]
struct FileBuf([u8; 64]);

// ---------- PR #83: x_open clears pMethods on failure ----------

struct ZeroHandle;
impl VfsHandle for ZeroHandle {
    fn readonly(&self) -> bool {
        false
    }
    fn in_memory(&self) -> bool {
        false
    }
}

struct AlwaysFailOpenVfs;
impl Vfs for AlwaysFailOpenVfs {
    type Handle = ZeroHandle;
    fn open(&self, _: Option<&str>, _: OpenOpts) -> VfsResult<Self::Handle> {
        Err(vars::SQLITE_CANTOPEN)
    }
    fn delete(&self, _: &str) -> VfsResult<()> {
        Ok(())
    }
    fn access(&self, _: &str, _: AccessFlags) -> VfsResult<bool> {
        Ok(false)
    }
    fn file_size(&self, _: &mut Self::Handle) -> VfsResult<usize> {
        Ok(0)
    }
    fn truncate(&self, _: &mut Self::Handle, _: usize) -> VfsResult<()> {
        Ok(())
    }
    fn write(&self, _: &mut Self::Handle, _: usize, d: &[u8]) -> VfsResult<usize> {
        Ok(d.len())
    }
    fn read(&self, _: &mut Self::Handle, _: usize, _: &mut [u8]) -> VfsResult<usize> {
        Ok(0)
    }
    fn lock(&self, _: &mut Self::Handle, _: LockLevel) -> VfsResult<()> {
        Ok(())
    }
    fn unlock(&self, _: &mut Self::Handle, _: LockLevel) -> VfsResult<()> {
        Ok(())
    }
    fn check_reserved_lock(&self, _: &mut Self::Handle) -> VfsResult<bool> {
        Ok(false)
    }
    fn close(&self, _: Self::Handle) -> VfsResult<()> {
        Ok(())
    }
}

#[test]
fn xopen_failure_sets_pmethods_null() {
    let name = unique_name("failopen");
    sqlite_plugin::vfs::register_static(
        name.clone(),
        AlwaysFailOpenVfs,
        RegisterOpts { make_default: false },
    )
    .expect("register");

    unsafe {
        let vfs = ffi::sqlite3_vfs_find(name.as_ptr());
        assert!(!vfs.is_null(), "vfs not registered");
        assert!(
            (*vfs).szOsFile as usize <= core::mem::size_of::<FileBuf>(),
            "FileBuf too small for szOsFile={}",
            (*vfs).szOsFile
        );

        // Pre-fill with 0xAA so an uninitialized pMethods would not look like NULL.
        let mut buf = Box::new(FileBuf([0xAA; 64]));
        let file_ptr = (&raw mut buf.0).cast::<ffi::sqlite3_file>();

        let path = CString::new("ignored.db").unwrap();
        let rc = (*vfs).xOpen.expect("xOpen")(
            vfs,
            path.as_ptr() as *const c_char,
            file_ptr,
            ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE,
            core::ptr::null_mut(),
        );
        assert_ne!(rc, ffi::SQLITE_OK, "open should have failed");
        assert!(
            (*file_ptr).pMethods.is_null(),
            "pMethods must be NULL after failed xOpen (got {:p})",
            (*file_ptr).pMethods,
        );
    }
}

// ---------- PR #84: x_read zero-fills the tail and reports SQLITE_IOERR_SHORT_READ ----------

struct ShortReadVfs {
    bytes: usize,
}
impl Vfs for ShortReadVfs {
    type Handle = ZeroHandle;
    fn open(&self, _: Option<&str>, _: OpenOpts) -> VfsResult<Self::Handle> {
        Ok(ZeroHandle)
    }
    fn delete(&self, _: &str) -> VfsResult<()> {
        Ok(())
    }
    fn access(&self, _: &str, _: AccessFlags) -> VfsResult<bool> {
        Ok(false)
    }
    fn file_size(&self, _: &mut Self::Handle) -> VfsResult<usize> {
        Ok(self.bytes)
    }
    fn truncate(&self, _: &mut Self::Handle, _: usize) -> VfsResult<()> {
        Ok(())
    }
    fn write(&self, _: &mut Self::Handle, _: usize, d: &[u8]) -> VfsResult<usize> {
        Ok(d.len())
    }
    fn read(&self, _: &mut Self::Handle, _: usize, buf: &mut [u8]) -> VfsResult<usize> {
        // Fill only the "read" prefix with a sentinel; leave the tail untouched
        // so we can verify the wrapper (not us) is the one zero-filling.
        let n = self.bytes.min(buf.len());
        buf[..n].fill(0xCC);
        Ok(n)
    }
    fn lock(&self, _: &mut Self::Handle, _: LockLevel) -> VfsResult<()> {
        Ok(())
    }
    fn unlock(&self, _: &mut Self::Handle, _: LockLevel) -> VfsResult<()> {
        Ok(())
    }
    fn check_reserved_lock(&self, _: &mut Self::Handle) -> VfsResult<bool> {
        Ok(false)
    }
    fn close(&self, _: Self::Handle) -> VfsResult<()> {
        Ok(())
    }
}

#[test]
fn xread_short_read_zero_fills_and_reports_status() {
    let name = unique_name("shortread");
    sqlite_plugin::vfs::register_static(
        name.clone(),
        ShortReadVfs { bytes: 4 },
        RegisterOpts { make_default: false },
    )
    .expect("register");

    unsafe {
        let vfs = ffi::sqlite3_vfs_find(name.as_ptr());
        assert!(!vfs.is_null());
        assert!((*vfs).szOsFile as usize <= core::mem::size_of::<FileBuf>());

        let mut buf = Box::new(FileBuf([0; 64]));
        let file_ptr = (&raw mut buf.0).cast::<ffi::sqlite3_file>();

        let path = CString::new("ignored.db").unwrap();
        let rc = (*vfs).xOpen.expect("xOpen")(
            vfs,
            path.as_ptr() as *const c_char,
            file_ptr,
            ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE,
            core::ptr::null_mut(),
        );
        assert_eq!(rc, ffi::SQLITE_OK);
        let methods = (*file_ptr).pMethods;
        assert!(!methods.is_null());

        // Pre-fill the read buffer with 0xAA so we can detect the wrapper's zero-fill.
        let mut read_buf = [0xAA_u8; 16];
        let rc = (*methods).xRead.expect("xRead")(
            file_ptr,
            read_buf.as_mut_ptr().cast::<c_void>(),
            read_buf.len() as c_int,
            0,
        );
        assert_eq!(
            rc,
            ffi::SQLITE_IOERR_SHORT_READ,
            "short read must surface SQLITE_IOERR_SHORT_READ",
        );
        assert_eq!(
            &read_buf[..4],
            &[0xCC; 4],
            "first {bytes} bytes should reflect what the underlying VFS wrote",
            bytes = 4,
        );
        assert!(
            read_buf[4..].iter().all(|&b| b == 0),
            "tail must be zero-filled (got {read_buf:?})",
        );

        // Close so the FileWrapper's handle is dropped properly.
        (*methods).xClose.expect("xClose")(file_ptr);
    }
}
