use std::collections::hash_map::Entry;
use std::mem;

use super::*;
use wip::WalIndex;

// Example mem-fs implementation:
// https://github.com/sqlite/sqlite/blob/a959bf53110bfada67a3a52187acd57aa2f34e19/ext/misc/memvfs.c

/// Close a file.
pub unsafe extern "C" fn close<V: Vfs, F: DatabaseHandle>(p_file: *mut ffi::sqlite3_file) -> c_int {
    if let Some(f) = (p_file as *mut FileState<V, F>).as_mut() {
        let ext = f.ext.assume_init_mut();
        if ext.delete_on_close {
            if let Err(err) = ext.vfs.delete(&ext.db_name) {
                return ext.set_last_error(ffi::SQLITE_DELETE, err);
            }
        }

        let ext = mem::replace(&mut f.ext, MaybeUninit::uninit());
        let ext = ext.assume_init(); // extract the value to drop it
        log::trace!("[{}] close ({})", ext.id, ext.db_name);
    }

    ffi::SQLITE_OK
}

/// Read data from a file.
pub unsafe extern "C" fn read<V, F: DatabaseHandle>(
    p_file: *mut ffi::sqlite3_file,
    z_buf: *mut c_void,
    i_amt: c_int,
    i_ofst: ffi::sqlite3_int64,
) -> c_int {
    let state = match file_state::<V, F>(p_file) {
        Ok(f) => f,
        Err(_) => return ffi::SQLITE_IOERR_CLOSE,
    };
    log::trace!(
        "[{}] read offset={} len={} ({})",
        state.id,
        i_ofst,
        i_amt,
        state.db_name
    );

    let out = slice::from_raw_parts_mut(z_buf as *mut u8, i_amt as usize);
    if let Err(err) = state.file.read_exact_at(out, i_ofst as u64) {
        let kind = err.kind();
        if kind == ErrorKind::UnexpectedEof {
            return ffi::SQLITE_IOERR_SHORT_READ;
        } else {
            return state.set_last_error(ffi::SQLITE_IOERR_READ, err);
        }
    }

    ffi::SQLITE_OK
}

/// Write data to a file.
pub unsafe extern "C" fn write<V, F: DatabaseHandle>(
    p_file: *mut ffi::sqlite3_file,
    z: *const c_void,
    i_amt: c_int,
    i_ofst: ffi::sqlite3_int64,
) -> c_int {
    let state = match file_state::<V, F>(p_file) {
        Ok(f) => f,
        Err(_) => return ffi::SQLITE_IOERR_WRITE,
    };
    log::trace!(
        "[{}] write offset={} len={} ({})",
        state.id,
        i_ofst,
        i_amt,
        state.db_name
    );

    let data = slice::from_raw_parts(z as *mut u8, i_amt as usize);
    let result = state.file.write_all_at(data, i_ofst as u64);

    match result {
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::WriteZero => {
            return ffi::SQLITE_FULL;
        }
        Err(err) => return state.set_last_error(ffi::SQLITE_IOERR_WRITE, err),
    }

    ffi::SQLITE_OK
}

/// Truncate a file.
pub unsafe extern "C" fn truncate<V, F: DatabaseHandle>(
    p_file: *mut ffi::sqlite3_file,
    size: ffi::sqlite3_int64,
) -> c_int {
    let state = match file_state::<V, F>(p_file) {
        Ok(f) => f,
        Err(_) => return ffi::SQLITE_IOERR_FSYNC,
    };

    let size: u64 = if let Some(chunk_size) = state.chunk_size {
        (((size as usize + chunk_size - 1) / chunk_size) * chunk_size) as u64
    } else {
        size as u64
    };

    log::trace!("[{}] truncate size={} ({})", state.id, size, state.db_name);

    if let Err(err) = state.file.set_len(size) {
        return state.set_last_error(ffi::SQLITE_IOERR_TRUNCATE, err);
    }

    ffi::SQLITE_OK
}

/// Persist changes to a file.
pub unsafe extern "C" fn sync<V, F: DatabaseHandle>(
    p_file: *mut ffi::sqlite3_file,
    flags: c_int,
) -> c_int {
    let state = match file_state::<V, F>(p_file) {
        Ok(f) => f,
        Err(_) => return ffi::SQLITE_IOERR_FSYNC,
    };
    log::trace!("[{}] sync ({})", state.id, state.db_name);

    if let Err(err) = state.file.sync(flags & ffi::SQLITE_SYNC_DATAONLY > 0) {
        return state.set_last_error(ffi::SQLITE_IOERR_FSYNC, err);
    }

    ffi::SQLITE_OK
}

/// Return the current file-size of a file.
pub unsafe extern "C" fn file_size<V, F: DatabaseHandle>(
    p_file: *mut ffi::sqlite3_file,
    p_size: *mut ffi::sqlite3_int64,
) -> c_int {
    let state = match file_state::<V, F>(p_file) {
        Ok(f) => f,
        Err(_) => return ffi::SQLITE_IOERR_FSTAT,
    };
    log::trace!("[{}] file_size ({})", state.id, state.db_name);

    if let Err(err) = state.file.size().and_then(|n| {
        let p_size: &mut ffi::sqlite3_int64 = p_size.as_mut().ok_or_else(null_ptr_error)?;
        *p_size = n as ffi::sqlite3_int64;
        Ok(())
    }) {
        return state.set_last_error(ffi::SQLITE_IOERR_FSTAT, err);
    }

    ffi::SQLITE_OK
}

/// Lock a file.
pub unsafe extern "C" fn lock<V, F: DatabaseHandle>(
    p_file: *mut ffi::sqlite3_file,
    e_lock: c_int,
) -> c_int {
    let state = match file_state::<V, F>(p_file) {
        Ok(f) => f,
        Err(_) => return ffi::SQLITE_IOERR_LOCK,
    };
    log::trace!("[{}] lock ({})", state.id, state.db_name);

    let lock = match LockKind::from_i32(e_lock) {
        Some(lock) => lock,
        None => return ffi::SQLITE_IOERR_LOCK,
    };
    match state.file.lock(lock) {
        Ok(true) => {
            state.has_exclusive_lock = lock == LockKind::Exclusive;
            log::trace!("[{}] lock={:?} ({})", state.id, lock, state.db_name);

            // If just acquired a exclusive database lock while not having any exclusive lock
            // on the wal index, make sure the wal index is up to date.
            if state.has_exclusive_lock {
                let has_exclusive_wal_index = state
                    .wal_index_locks
                    .iter()
                    .any(|(_, lock)| *lock == wip::WalIndexLock::Exclusive);

                if !has_exclusive_wal_index {
                    log::trace!(
                        "[{}] acquired exclusive db lock, pulling wal index changes",
                        state.id,
                    );

                    if let Some((wal_index, _)) = state.wal_index.as_mut() {
                        for (region, data) in &mut state.wal_index_regions {
                            if let Err(err) = wal_index.pull(*region as u32, data) {
                                log::error!(
                                    "[{}] pulling wal index changes failed: {}",
                                    state.id,
                                    err
                                )
                            }
                        }
                    }
                }
            }

            ffi::SQLITE_OK
        }
        Ok(false) => {
            log::trace!(
                "[{}] busy (denied {:?}) ({})",
                state.id,
                lock,
                state.db_name
            );
            ffi::SQLITE_BUSY
        }
        Err(err) => state.set_last_error(ffi::SQLITE_IOERR_LOCK, err),
    }
}

/// Unlock a file.
pub unsafe extern "C" fn unlock<V, F: DatabaseHandle>(
    p_file: *mut ffi::sqlite3_file,
    e_lock: c_int,
) -> c_int {
    let state = match file_state::<V, F>(p_file) {
        Ok(f) => f,
        Err(_) => return ffi::SQLITE_IOERR_UNLOCK,
    };
    log::trace!("[{}] unlock ({})", state.id, state.db_name);

    let lock = match LockKind::from_i32(e_lock) {
        Some(lock) => lock,
        None => return ffi::SQLITE_IOERR_UNLOCK,
    };
    match state.file.unlock(lock) {
        Ok(true) => {
            state.has_exclusive_lock = lock == LockKind::Exclusive;
            log::trace!("[{}] unlock={:?} ({})", state.id, lock, state.db_name);
            ffi::SQLITE_OK
        }
        Ok(false) => ffi::SQLITE_BUSY,
        Err(err) => state.set_last_error(ffi::SQLITE_IOERR_UNLOCK, err),
    }
}

/// Check if another file-handle holds a [LockKind::Reserved] lock on a file.
pub unsafe extern "C" fn check_reserved_lock<V, F: DatabaseHandle>(
    p_file: *mut ffi::sqlite3_file,
    p_res_out: *mut c_int,
) -> c_int {
    let state = match file_state::<V, F>(p_file) {
        Ok(f) => f,
        Err(_) => return ffi::SQLITE_IOERR_CHECKRESERVEDLOCK,
    };
    log::trace!("[{}] check_reserved_lock ({})", state.id, state.db_name);

    if let Err(err) = state.file.reserved().and_then(|is_reserved| {
        let p_res_out: &mut c_int = p_res_out.as_mut().ok_or_else(null_ptr_error)?;
        *p_res_out = is_reserved as c_int;
        Ok(())
    }) {
        return state.set_last_error(ffi::SQLITE_IOERR_UNLOCK, err);
    }

    ffi::SQLITE_OK
}

/// File control method. For custom operations on a mem-file.
pub unsafe extern "C" fn file_control<V: Vfs, F: DatabaseHandle>(
    p_file: *mut ffi::sqlite3_file,
    op: c_int,
    p_arg: *mut c_void,
) -> c_int {
    let state = match file_state::<V, F>(p_file) {
        Ok(f) => f,
        Err(_) => return ffi::SQLITE_NOTFOUND,
    };
    log::trace!("[{}] file_control op={} ({})", state.id, op, state.db_name);

    // Docs: https://www.sqlite.org/c3ref/c_fcntl_begin_atomic_write.html
    match op {
        // The following op codes are alreay handled by sqlite before, so no need to handle them
        // in a custom VFS.
        ffi::SQLITE_FCNTL_FILE_POINTER
        | ffi::SQLITE_FCNTL_VFS_POINTER
        | ffi::SQLITE_FCNTL_JOURNAL_POINTER
        | ffi::SQLITE_FCNTL_DATA_VERSION
        | ffi::SQLITE_FCNTL_RESERVE_BYTES => ffi::SQLITE_NOTFOUND,

        // The following op codes are no longer used and thus ignored.
        ffi::SQLITE_FCNTL_SYNC_OMITTED => ffi::SQLITE_NOTFOUND,

        // Used for debugging. Write current state of the lock into (int)pArg.
        ffi::SQLITE_FCNTL_LOCKSTATE => match state.file.current_lock() {
            Ok(lock) => {
                if let Some(p_arg) = (p_arg as *mut i32).as_mut() {
                    *p_arg = lock as i32;
                }
                ffi::SQLITE_OK
            }
            Err(err) => state.set_last_error(ffi::SQLITE_ERROR, err),
        },

        // Relevant for proxy-type locking. Not implemented.
        ffi::SQLITE_FCNTL_GET_LOCKPROXYFILE | ffi::SQLITE_FCNTL_SET_LOCKPROXYFILE => {
            ffi::SQLITE_NOTFOUND
        }

        // Write last error number into (int)pArg.
        ffi::SQLITE_FCNTL_LAST_ERRNO => {
            if let Some(p_arg) = (p_arg as *mut i32).as_mut() {
                *p_arg = state.last_errno;
            }
            ffi::SQLITE_OK
        }

        // Give the VFS layer a hint of how large the database file will grow to be during the
        // current transaction.
        ffi::SQLITE_FCNTL_SIZE_HINT => {
            let size_hint = match (p_arg as *mut i64)
                .as_ref()
                .cloned()
                .and_then(|s| u64::try_from(s).ok())
            {
                Some(chunk_size) => chunk_size,
                None => {
                    return state.set_last_error(
                        ffi::SQLITE_NOTFOUND,
                        std::io::Error::new(ErrorKind::Other, "expect size hint arg"),
                    );
                }
            };

            let current = match state.file.size() {
                Ok(size) => size,
                Err(err) => return state.set_last_error(ffi::SQLITE_ERROR, err),
            };

            if current > size_hint {
                return ffi::SQLITE_OK;
            }

            if let Some(chunk_size) = state.chunk_size {
                let chunk_size = chunk_size as u64;
                let size = ((size_hint + chunk_size - 1) / chunk_size) * chunk_size;
                if let Err(err) = state.file.set_len(size) {
                    return state.set_last_error(ffi::SQLITE_IOERR_TRUNCATE, err);
                }
            } else if let Err(err) = state.file.set_len(size_hint) {
                return state.set_last_error(ffi::SQLITE_IOERR_TRUNCATE, err);
            }

            ffi::SQLITE_OK
        }

        // Request that the VFS extends and truncates the database file in chunks of a size
        // specified by the user. Return an error as this is not forwarded to the [Vfs] trait
        // right now.
        ffi::SQLITE_FCNTL_CHUNK_SIZE => {
            let chunk_size = match (p_arg as *mut i32)
                .as_ref()
                .cloned()
                .and_then(|s| usize::try_from(s).ok())
            {
                Some(chunk_size) => chunk_size,
                None => {
                    return state.set_last_error(
                        ffi::SQLITE_NOTFOUND,
                        std::io::Error::new(ErrorKind::Other, "expect chunk_size arg"),
                    );
                }
            };

            if let Err(err) = state.file.set_chunk_size(chunk_size) {
                return state.set_last_error(ffi::SQLITE_ERROR, err);
            }

            state.chunk_size = Some(chunk_size);

            ffi::SQLITE_OK
        }

        // Configure automatic retry counts and intervals for certain disk I/O operations for
        // the windows VFS in order to provide robustness in the presence of anti-virus
        // programs. Not implemented.
        ffi::SQLITE_FCNTL_WIN32_AV_RETRY => ffi::SQLITE_NOTFOUND,

        // Enable or disable the persistent WAL setting.
        ffi::SQLITE_FCNTL_PERSIST_WAL => {
            if let Some(p_arg) = (p_arg as *mut i32).as_mut() {
                if *p_arg < 0 {
                    // query current setting
                    *p_arg = state.persist_wal as i32;
                } else {
                    state.persist_wal = *p_arg == 1;
                }
            };

            ffi::SQLITE_OK
        }

        // Indicate that, unless it is rolled back for some reason, the entire database file
        // will be overwritten by the current transaction. Not implemented.
        ffi::SQLITE_FCNTL_OVERWRITE => ffi::SQLITE_NOTFOUND,

        // Used to obtain the names of all VFSes in the VFS stack.
        ffi::SQLITE_FCNTL_VFSNAME => {
            if let Some(p_arg) = (p_arg as *mut *const c_char).as_mut() {
                let name = ManuallyDrop::new(state.vfs_name.clone());
                *p_arg = name.as_ptr();
            };

            ffi::SQLITE_OK
        }

        // Set or query the persistent "powersafe-overwrite" or "PSOW" setting.
        ffi::SQLITE_FCNTL_POWERSAFE_OVERWRITE => {
            if let Some(p_arg) = (p_arg as *mut i32).as_mut() {
                if *p_arg < 0 {
                    // query current setting
                    *p_arg = state.powersafe_overwrite as i32;
                } else {
                    state.powersafe_overwrite = *p_arg == 1;
                }
            };

            ffi::SQLITE_OK
        }

        // Optionally intercept PRAGMA statements. Always fall back to normal pragma processing.
        ffi::SQLITE_FCNTL_PRAGMA => ffi::SQLITE_NOTFOUND,

        // May be invoked by SQLite on the database file handle shortly after it is opened in
        // order to provide a custom VFS with access to the connection's busy-handler callback.
        // Not implemented.
        ffi::SQLITE_FCNTL_BUSYHANDLER => ffi::SQLITE_NOTFOUND,

        // Generate a temporary filename. Not implemented.
        ffi::SQLITE_FCNTL_TEMPFILENAME => {
            if let Some(p_arg) = (p_arg as *mut *const c_char).as_mut() {
                let name = state.vfs.temporary_name();
                // unwrap() is fine as os strings are an arbitrary sequences of non-zero bytes
                let name = CString::new(name.as_bytes()).unwrap();
                let name = ManuallyDrop::new(name);
                *p_arg = name.as_ptr();
            };

            ffi::SQLITE_OK
        }

        // Query or set the maximum number of bytes that will be used for memory-mapped I/O.
        // Not implemented.
        ffi::SQLITE_FCNTL_MMAP_SIZE => ffi::SQLITE_NOTFOUND,

        // Advisory information to the VFS about what the higher layers of the SQLite stack are
        // doing.
        ffi::SQLITE_FCNTL_TRACE => {
            let trace = CStr::from_ptr(p_arg as *const c_char);
            log::trace!("{}", trace.to_string_lossy());
            ffi::SQLITE_OK
        }

        // Check whether or not the file has been renamed, moved, or deleted since it was first
        // opened.
        ffi::SQLITE_FCNTL_HAS_MOVED => match state.file.moved() {
            Ok(moved) => {
                if let Some(p_arg) = (p_arg as *mut i32).as_mut() {
                    *p_arg = moved as i32;
                }
                ffi::SQLITE_OK
            }
            Err(err) => state.set_last_error(ffi::SQLITE_ERROR, err),
        },

        // Sent to the VFS immediately before the xSync method is invoked on a database file
        // descriptor. Silently ignored.
        ffi::SQLITE_FCNTL_SYNC => ffi::SQLITE_OK,

        // Sent to the VFS after a transaction has been committed immediately but before the
        // database is unlocked. Silently ignored.
        ffi::SQLITE_FCNTL_COMMIT_PHASETWO => ffi::SQLITE_OK,

        // Used for debugging. Swap the file handle with the one pointed to by the pArg
        // argument. This capability is used during testing and only needs to be supported when
        // SQLITE_TEST is defined. Not implemented.
        ffi::SQLITE_FCNTL_WIN32_SET_HANDLE => ffi::SQLITE_NOTFOUND,

        // Signal to the VFS layer that it might be advantageous to block on the next WAL lock
        // if the lock is not immediately available. The WAL subsystem issues this signal during
        // rare circumstances in order to fix a problem with priority inversion.
        // Not implemented.
        ffi::SQLITE_FCNTL_WAL_BLOCK => ffi::SQLITE_NOTFOUND,

        // Implemented by zipvfs only.
        ffi::SQLITE_FCNTL_ZIPVFS => ffi::SQLITE_NOTFOUND,

        // Implemented by the special VFS used by the RBU extension only.
        ffi::SQLITE_FCNTL_RBU => ffi::SQLITE_NOTFOUND,

        // Obtain the underlying native file handle associated with a file handle.
        // Not implemented.
        ffi::SQLITE_FCNTL_WIN32_GET_HANDLE => ffi::SQLITE_NOTFOUND,

        // Usage is not documented. Not implemented.
        ffi::SQLITE_FCNTL_PDB => ffi::SQLITE_NOTFOUND,

        // Used for "batch write mode". Not supported.
        ffi::SQLITE_FCNTL_BEGIN_ATOMIC_WRITE
        | ffi::SQLITE_FCNTL_COMMIT_ATOMIC_WRITE
        | ffi::SQLITE_FCNTL_ROLLBACK_ATOMIC_WRITE => ffi::SQLITE_NOTFOUND,

        // Configure a VFS to block for up to M milliseconds before failing when attempting to
        // obtain a file lock using the xLock or xShmLock methods of the VFS. Not implemented.
        ffi::SQLITE_FCNTL_LOCK_TIMEOUT => ffi::SQLITE_NOTFOUND,

        // Used by in-memory VFS.
        ffi::SQLITE_FCNTL_SIZE_LIMIT => ffi::SQLITE_NOTFOUND,

        // Invoked from within a checkpoint in wal mode after the client has finished copying
        // pages from the wal file to the database file, but before the *-shm file is updated to
        // record the fact that the pages have been checkpointed. Silently ignored.
        ffi::SQLITE_FCNTL_CKPT_DONE => ffi::SQLITE_OK,

        // Invoked from within a checkpoint in wal mode before the client starts to copy pages
        // from the wal file to the database file. Silently ignored.
        ffi::SQLITE_FCNTL_CKPT_START => ffi::SQLITE_OK,

        // Detect whether or not there is a database client in another process with a wal-mode
        // transaction open on the database or not. Not implemented because it is a
        // unix-specific feature.
        ffi::SQLITE_FCNTL_EXTERNAL_READER => ffi::SQLITE_NOTFOUND,

        // Unknown use-case. Ignored.
        ffi::SQLITE_FCNTL_CKSM_FILE => ffi::SQLITE_NOTFOUND,

        _ => ffi::SQLITE_NOTFOUND,
    }
}

/// Return the sector-size in bytes for a file.
pub unsafe extern "C" fn sector_size<V, F: DatabaseHandle>(
    p_file: *mut ffi::sqlite3_file,
) -> c_int {
    log::trace!("sector_size");

    let state = match file_state::<V, F>(p_file) {
        Ok(f) => f,
        Err(_) => return ffi::SQLITE_NOTFOUND,
    };

    match state.file.get_sector_size() {
        Ok(sector_size) => sector_size,
        Err(err) => state.set_last_error(ffi::SQLITE_ERROR, err),
    }
}

/// Return the device characteristic flags supported by a file.
pub unsafe extern "C" fn device_characteristics<V, F: DatabaseHandle>(
    p_file: *mut ffi::sqlite3_file,
) -> c_int {
    let state = match file_state::<V, F>(p_file) {
        Ok(f) => f,
        Err(_) => return ffi::SQLITE_IOERR_SHMMAP,
    };

    log::trace!("[{}] device_characteristics", state.id,);

    // The following characteristics are needed to match the expected behavior of the tests.

    // after reboot following a crash or power loss, the only bytes in a file that were written
    // at the application level might have changed and that adjacent bytes, even bytes within
    // the same sector are guaranteed to be unchanged
    if state.powersafe_overwrite {
        ffi::SQLITE_IOCAP_POWERSAFE_OVERWRITE
    } else {
        0
    }
}

/// Create a shared memory file mapping.
pub unsafe extern "C" fn shm_map<V, F: DatabaseHandle>(
    p_file: *mut ffi::sqlite3_file,
    region_ix: i32,
    region_size: i32,
    b_extend: i32,
    pp: *mut *mut c_void,
) -> i32 {
    let state = match file_state::<V, F>(p_file) {
        Ok(f) => f,
        Err(_) => return ffi::SQLITE_IOERR_SHMMAP,
    };
    log::trace!(
        "[{}] shm_map pg={} sz={} extend={} ({})",
        state.id,
        region_ix,
        region_size,
        b_extend,
        state.db_name
    );

    if !F::WalIndex::enabled() {
        return ffi::SQLITE_IOERR_SHMLOCK;
    }

    if region_size != 32768 {
        return state.set_last_error(
            ffi::SQLITE_IOERR_SHMMAP,
            std::io::Error::new(
                ErrorKind::Other,
                format!(
                    "encountered region size other than 32kB; got {}",
                    region_size
                ),
            ),
        );
    }

    let (wal_index, readonly) = match state.wal_index.as_mut() {
        Some((wal_index, readonly)) => (wal_index, *readonly),
        None => {
            let (wal_index, readonly) = state.wal_index.get_or_insert(
                match state
                    .file
                    .wal_index(false)
                    .map(|wal_index| (wal_index, false))
                    .or_else(|err| {
                        if err.kind() == ErrorKind::PermissionDenied {
                            // Try again as readonly
                            state
                                .file
                                .wal_index(true)
                                .map(|wal_index| (wal_index, true))
                                .map_err(|_| err)
                        } else {
                            Err(err)
                        }
                    }) {
                    Ok((wal_index, readonly)) => (wal_index, readonly),
                    Err(err) => {
                        return state.set_last_error(ffi::SQLITE_IOERR_SHMMAP, err);
                    }
                },
            );
            (wal_index, *readonly)
        }
    };

    let entry = state.wal_index_regions.entry(region_ix as u32);
    match entry {
        Entry::Occupied(mut entry) => {
            *pp = entry.get_mut().as_mut_ptr() as *mut c_void;
        }
        Entry::Vacant(entry) => {
            let mut m = match wal_index.map(region_ix as u32) {
                Ok(m) => Box::pin(m),
                Err(err) => {
                    return state.set_last_error(ffi::SQLITE_IOERR_SHMMAP, err);
                }
            };
            *pp = m.as_mut_ptr() as *mut c_void;
            entry.insert(m);
        }
    }

    if readonly {
        ffi::SQLITE_READONLY
    } else {
        ffi::SQLITE_OK
    }
}

/// Perform locking on a shared-memory segment.
pub unsafe extern "C" fn shm_lock<V, F: DatabaseHandle>(
    p_file: *mut ffi::sqlite3_file,
    offset: i32,
    n: i32,
    flags: i32,
) -> i32 {
    let state = match file_state::<V, F>(p_file) {
        Ok(f) => f,
        Err(_) => return ffi::SQLITE_IOERR_SHMMAP,
    };
    let locking = flags & ffi::SQLITE_SHM_LOCK > 0;
    let exclusive = flags & ffi::SQLITE_SHM_EXCLUSIVE > 0;
    log::trace!(
        "[{}] shm_lock offset={} n={} lock={} exclusive={} (flags={}) ({})",
        state.id,
        offset,
        n,
        locking,
        exclusive,
        flags,
        state.db_name
    );

    let range = offset as u8..(offset + n) as u8;
    let lock = match (locking, exclusive) {
        (true, true) => wip::WalIndexLock::Exclusive,
        (true, false) => wip::WalIndexLock::Shared,
        (false, _) => wip::WalIndexLock::None,
    };

    let (wal_index, readonly) = match state.wal_index.as_mut() {
        Some((wal_index, readonly)) => (wal_index, *readonly),
        None => {
            return state.set_last_error(
                ffi::SQLITE_IOERR_SHMLOCK,
                std::io::Error::new(
                    ErrorKind::Other,
                    "trying to lock wal index, which isn't created yet",
                ),
            )
        }
    };

    if locking {
        let has_exclusive = state
            .wal_index_locks
            .iter()
            .any(|(_, lock)| *lock == wip::WalIndexLock::Exclusive);

        if !has_exclusive {
            log::trace!(
                "[{}] does not have wal index write lock, pulling changes",
                state.id
            );
            for (region, data) in &mut state.wal_index_regions {
                if let Err(err) = wal_index.pull(*region as u32, data) {
                    return state.set_last_error(ffi::SQLITE_IOERR_SHMLOCK, err);
                }
            }
        }
    } else {
        let releases_any_exclusive = state
            .wal_index_locks
            .iter()
            .any(|(region, lock)| *lock == wip::WalIndexLock::Exclusive && range.contains(region));

        // push index changes when moving from any exclusive lock to no exclusive locks
        if releases_any_exclusive && !readonly {
            log::trace!(
                "[{}] releasing an exclusive lock, pushing wal index changes",
                state.id,
            );
            for (region, data) in &mut state.wal_index_regions {
                if let Err(err) = wal_index.push(*region as u32, data) {
                    return state.set_last_error(ffi::SQLITE_IOERR_SHMLOCK, err);
                }
            }
        }
    }

    match wal_index.lock(range.clone(), lock) {
        Ok(true) => {
            for region in range {
                state.wal_index_locks.insert(region, lock);
            }
            ffi::SQLITE_OK
        }
        Ok(false) => ffi::SQLITE_BUSY,
        Err(err) => state.set_last_error(ffi::SQLITE_IOERR_SHMLOCK, err),
    }
}

/// Memory barrier operation on shared memory.
pub unsafe extern "C" fn shm_barrier<V, F: DatabaseHandle>(p_file: *mut ffi::sqlite3_file) {
    let state = match file_state::<V, F>(p_file) {
        Ok(f) => f,
        Err(_) => return,
    };
    log::trace!("[{}] shm_barrier ({})", state.id, state.db_name);

    let (wal_index, readonly) = if let Some((wal_index, readonly)) = state.wal_index.as_mut() {
        (wal_index, *readonly)
    } else {
        return;
    };

    if state.has_exclusive_lock && !readonly {
        log::trace!(
            "[{}] has exclusive db lock, pushing wal index changes",
            state.id,
        );
        for (region, data) in &mut state.wal_index_regions {
            if let Err(err) = wal_index.push(*region as u32, data) {
                log::error!("[{}] pushing wal index changes failed: {}", state.id, err)
            }
        }

        return;
    }

    let has_exclusive = state
        .wal_index_locks
        .iter()
        .any(|(_, lock)| *lock == wip::WalIndexLock::Exclusive);

    if !has_exclusive {
        log::trace!(
            "[{}] does not have wal index write lock, pulling changes",
            state.id
        );
        for (region, data) in &mut state.wal_index_regions {
            if let Err(err) = wal_index.pull(*region as u32, data) {
                log::error!("[{}] pulling wal index changes failed: {}", state.id, err)
            }
        }
    }
}

/// Unmap a shared memory segment.
pub unsafe extern "C" fn shm_unmap<V, F: DatabaseHandle>(
    p_file: *mut ffi::sqlite3_file,
    delete_flags: i32,
) -> i32 {
    let state = match file_state::<V, F>(p_file) {
        Ok(f) => f,
        Err(_) => return ffi::SQLITE_IOERR_SHMMAP,
    };
    log::trace!(
        "[{}] shm_unmap delete={} ({})",
        state.id,
        delete_flags == 1,
        state.db_name
    );

    state.wal_index_regions.clear();
    state.wal_index_locks.clear();

    if delete_flags == 1 {
        if let Some((wal_index, readonly)) = state.wal_index.take() {
            if !readonly {
                if let Err(err) = wal_index.delete() {
                    return state.set_last_error(ffi::SQLITE_ERROR, err);
                }
            }
        }
    }

    ffi::SQLITE_OK
}
