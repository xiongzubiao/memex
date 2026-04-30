//! Filesystem-type detection for the watcher's native-vs-polling
//! decision. Native watchers (inotify, FSEvents, ReadDirectoryChangesW)
//! don't deliver events reliably across network mounts, so memex falls
//! back to periodic polling there. We probe via `statfs` on Linux/macOS;
//! Windows is a stub (returns false) until someone adds GetDriveType.

use std::path::Path;

/// Returns `true` if the path lives on a known network filesystem.
/// On detection failure (path missing, syscall error, unknown FS) we
/// return `false` and let the native watcher try — its own init
/// failure path already falls back to polling.
pub fn is_network_fs(path: &Path) -> bool {
    detect(path).unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn detect(path: &Path) -> Option<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statfs` writes into the zeroed struct on success and
    // returns 0; on error it returns -1 and leaves the struct
    // unspecified. We only read the struct on the 0-return path.
    let mut buf = unsafe { std::mem::zeroed::<libc::statfs>() };
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        return None;
    }
    // Magic numbers from <linux/magic.h>. f_type is signed on some
    // arches (i64) and unsigned on others, so cast to u64 first.
    let f_type = buf.f_type as u64;
    Some(matches!(
        f_type,
        // NFS
        0x6969
        // CIFS / SMB1
        | 0xff53_4d42
        // SMB2
        | 0xfe53_4d42
        // AFS (OpenAFS, kAFS)
        | 0x5346_414f | 0x6b41_4653
        // 9P (WSL2, QEMU/virtfs)
        | 0x0102_1997
    ))
}

#[cfg(target_os = "macos")]
fn detect(path: &Path) -> Option<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf = unsafe { std::mem::zeroed::<libc::statfs>() };
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        return None;
    }
    // f_fstypename is a fixed-size [c_char; 16] containing a C string
    // like "nfs", "smbfs", "afpfs", "webdav". Read up to the first NUL.
    let bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(buf.f_fstypename.as_ptr() as *const u8, buf.f_fstypename.len())
    };
    let nul = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let name = std::str::from_utf8(&bytes[..nul]).ok()?;
    Some(matches!(name, "nfs" | "smbfs" | "afpfs" | "webdav"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn detect(_path: &Path) -> Option<bool> {
    // Windows GetDriveType / DRIVE_REMOTE detection lives behind a
    // future cfg arm; for now non-Linux/macOS hosts fall through to
    // the native watcher and its own polling fallback on init failure.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_tmpdir_is_not_network() {
        let tmp = tempfile::tempdir().unwrap();
        // /tmp is tmpfs on most Linux setups, APFS on macOS — both
        // local. The probe should agree.
        assert!(!is_network_fs(tmp.path()));
    }

    #[test]
    fn missing_path_falls_through_to_false() {
        // statfs on a non-existent path returns ENOENT; we swallow
        // the error and let the native watcher's own fallback handle
        // any post-startup mount changes.
        assert!(!is_network_fs(Path::new("/nonexistent/memex/path")));
    }
}
