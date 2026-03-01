// cargo build --example memvfs --features dynamic

use std::{ffi::c_void, os::raw::c_char, ptr::NonNull, sync::Arc};

use parking_lot::Mutex;
use sqlite_plugin::{
    flags::{AccessFlags, LockLevel, OpenOpts, ShmLockMode},
    logger::{SqliteLogLevel, SqliteLogger},
    sqlite3_api_routines, vars,
    vfs::{Pragma, PragmaErr, RegisterOpts, Vfs, VfsHandle, VfsResult, register_dynamic},
};

#[derive(Debug, Clone)]
struct File {
    name: Option<String>,
    data: Arc<Mutex<Vec<u8>>>,
    /// Single shared-memory page used for WAL index.
    shm: Arc<Mutex<Option<Vec<u8>>>>,
    delete_on_close: bool,
    opts: OpenOpts,
}

impl File {
    fn is_named(&self, s: &str) -> bool {
        self.name.as_ref().is_some_and(|f| f == s)
    }
}

impl VfsHandle for File {
    fn readonly(&self) -> bool {
        self.opts.mode().is_readonly()
    }

    fn in_memory(&self) -> bool {
        true
    }
}

struct MemVfs {
    files: Arc<Mutex<Vec<File>>>,
}

impl Vfs for MemVfs {
    type Handle = File;

    fn open(&self, path: Option<&str>, opts: OpenOpts) -> VfsResult<Self::Handle> {
        log::debug!("open: path={:?}, opts={:?}", path, opts);
        let mode = opts.mode();
        if mode.is_readonly() {
            // readonly makes no sense since an in-memory VFS is not backed by
            // any pre-existing data.
            return Err(vars::SQLITE_CANTOPEN);
        }

        if let Some(path) = path {
            let mut files = self.files.lock();

            for file in files.iter() {
                if file.is_named(path) {
                    if mode.must_create() {
                        return Err(vars::SQLITE_CANTOPEN);
                    }
                    return Ok(file.clone());
                }
            }

            let file = File {
                name: Some(path.to_owned()),
                data: Default::default(),
                shm: Default::default(),
                delete_on_close: opts.delete_on_close(),
                opts,
            };
            files.push(file.clone());
            Ok(file)
        } else {
            let file = File {
                name: None,
                data: Default::default(),
                shm: Default::default(),
                delete_on_close: opts.delete_on_close(),
                opts,
            };
            Ok(file)
        }
    }

    fn delete(&self, path: &str) -> VfsResult<()> {
        log::debug!("delete: path={}", path);
        let mut found = false;
        self.files.lock().retain(|file| {
            if file.is_named(path) {
                found = true;
                false
            } else {
                true
            }
        });
        if !found {
            return Err(vars::SQLITE_IOERR_DELETE_NOENT);
        }
        Ok(())
    }

    fn access(&self, path: &str, flags: AccessFlags) -> VfsResult<bool> {
        log::debug!("access: path={}, flags={:?}", path, flags);
        Ok(self.files.lock().iter().any(|f| f.is_named(path)))
    }

    fn file_size(&self, handle: &mut Self::Handle) -> VfsResult<usize> {
        log::debug!("file_size: file={:?}", handle.name);
        Ok(handle.data.lock().len())
    }

    fn truncate(&self, handle: &mut Self::Handle, size: usize) -> VfsResult<()> {
        log::debug!("truncate: file={:?}, size={}", handle.name, size);
        let mut data = handle.data.lock();
        if size > data.len() {
            data.resize(size, 0);
        } else {
            data.truncate(size);
        }
        Ok(())
    }

    fn lock(&self, handle: &mut Self::Handle, level: LockLevel) -> VfsResult<()> {
        log::debug!("lock: file={:?}, level={:?}", handle.name, level);
        Ok(())
    }

    fn unlock(&self, handle: &mut Self::Handle, level: LockLevel) -> VfsResult<()> {
        log::debug!("unlock: file={:?}, level={:?}", handle.name, level);
        Ok(())
    }

    fn check_reserved_lock(&self, handle: &mut Self::Handle) -> VfsResult<bool> {
        log::debug!("check_reserved_lock: file={:?}", handle.name);
        Ok(false)
    }

    fn write(&self, handle: &mut Self::Handle, offset: usize, buf: &[u8]) -> VfsResult<usize> {
        log::debug!(
            "write: file={:?}, offset={}, len={}",
            handle.name,
            offset,
            buf.len()
        );
        let mut data = handle.data.lock();
        if offset + buf.len() > data.len() {
            data.resize(offset + buf.len(), 0);
        }
        data[offset..offset + buf.len()].copy_from_slice(buf);
        Ok(buf.len())
    }

    fn read(&self, handle: &mut Self::Handle, offset: usize, buf: &mut [u8]) -> VfsResult<usize> {
        log::debug!(
            "read: file={:?}, offset={}, len={}",
            handle.name,
            offset,
            buf.len()
        );
        let data = handle.data.lock();
        if offset > data.len() {
            return Ok(0);
        }
        let len = buf.len().min(data.len() - offset);
        buf[..len].copy_from_slice(&data[offset..offset + len]);
        Ok(len)
    }

    fn sync(&self, handle: &mut Self::Handle) -> VfsResult<()> {
        log::debug!("sync: file={:?}", handle.name);
        Ok(())
    }

    fn close(&self, handle: Self::Handle) -> VfsResult<()> {
        log::debug!("close: file={:?}", handle.name);
        if handle.delete_on_close {
            if let Some(ref name) = handle.name {
                self.delete(name)?;
            }
        }
        Ok(())
    }

    fn pragma(
        &self,
        handle: &mut Self::Handle,
        pragma: Pragma<'_>,
    ) -> Result<Option<String>, PragmaErr> {
        log::debug!("pragma: file={:?}, pragma={:?}", handle.name, pragma);
        Err(PragmaErr::NotFound)
    }

    fn shm_map(
        &self,
        handle: &mut Self::Handle,
        region_idx: usize,
        region_size: usize,
        extend: bool,
    ) -> VfsResult<Option<NonNull<u8>>> {
        log::debug!(
            "shm_map: file={:?}, region_idx={}, region_size={}, extend={}",
            handle.name,
            region_idx,
            region_size,
            extend
        );

        assert_eq!(region_idx, 0, "memvfs only supports a single shm region");

        let mut shm = handle.shm.lock();
        if shm.is_none() {
            if !extend {
                return Ok(None);
            }
            *shm = Some(vec![0u8; region_size]);
        }
        let buf = shm.as_mut().unwrap();
        Ok(NonNull::new(buf.as_mut_ptr()))
    }

    fn shm_lock(
        &self,
        handle: &mut Self::Handle,
        offset: u32,
        count: u32,
        mode: ShmLockMode,
    ) -> VfsResult<()> {
        log::debug!(
            "shm_lock: file={:?}, offset={}, count={}, mode={:?}",
            handle.name,
            offset,
            count,
            mode
        );
        // No-op: single-process in-memory VFS needs no locking.
        Ok(())
    }

    fn shm_barrier(&self, handle: &mut Self::Handle) {
        log::debug!("shm_barrier: file={:?}", handle.name);
        // No-op: single-process, no cross-process memory ordering needed.
    }

    fn shm_unmap(&self, handle: &mut Self::Handle, delete: bool) -> VfsResult<()> {
        log::debug!("shm_unmap: file={:?}, delete={}", handle.name, delete);
        if delete {
            *handle.shm.lock() = None;
        }
        Ok(())
    }
}

fn setup_logger(logger: SqliteLogger) {
    struct LogCompat {
        logger: Mutex<SqliteLogger>,
    }

    impl log::Log for LogCompat {
        fn enabled(&self, _metadata: &log::Metadata) -> bool {
            true
        }

        fn log(&self, record: &log::Record) {
            let level = match record.level() {
                log::Level::Error => SqliteLogLevel::Error,
                log::Level::Warn => SqliteLogLevel::Warn,
                _ => SqliteLogLevel::Notice,
            };
            let msg = format!("{}", record.args());
            self.logger.lock().log(level, &msg);
        }

        fn flush(&self) {}
    }

    let log = LogCompat { logger: Mutex::new(logger) };
    log::set_boxed_logger(Box::new(log)).expect("failed to setup global logger");
}

/// This function is called by `SQLite` when the extension is loaded. It registers
/// the memvfs VFS with `SQLite`.
/// # Safety
/// This function should only be called by sqlite's extension loading mechanism.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_memvfs_init(
    _db: *mut c_void,
    _pz_err_msg: *mut *mut c_char,
    p_api: *mut sqlite3_api_routines,
) -> std::os::raw::c_int {
    match unsafe {
        register_dynamic(
            p_api,
            c"mem".to_owned(),
            MemVfs { files: Default::default() },
            RegisterOpts { make_default: true },
        )
    } {
        Ok(logger) => setup_logger(logger),
        Err(err) => return err,
    };

    // set the log level to trace
    log::set_max_level(log::LevelFilter::Trace);

    vars::SQLITE_OK_LOAD_PERMANENTLY
}
