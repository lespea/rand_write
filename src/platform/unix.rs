//! Pieces shared by Linux and macOS

use std::fs::File;
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result, bail};

pub fn is_device(p: &Path) -> Result<bool> {
    use std::os::unix::fs::FileTypeExt;

    match std::fs::metadata(p) {
        Ok(m) => {
            let ft = m.file_type();
            Ok(ft.is_block_device() || ft.is_char_device())
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("couldn't stat {}", p.display())),
    }
}

pub fn free_space(fh: &File, _p: &Path) -> std::io::Result<u64> {
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: fstatvfs fills in the whole struct on success
    let st = unsafe {
        if libc::fstatvfs(fh.as_raw_fd(), st.as_mut_ptr()) == -1 {
            return Err(std::io::Error::last_os_error());
        }
        st.assume_init()
    };
    // Field widths differ between Linux and macOS
    #[allow(clippy::useless_conversion)]
    Ok(u64::from(st.f_bavail) * u64::from(st.f_frsize))
}

/// Adds context to an open result, turning the EBUSY an in-use disk is refused with into an
/// actionable error
pub fn check_open(
    res: std::io::Result<File>,
    p: &Path,
    device: bool,
    in_use_hint: &str,
) -> Result<File> {
    match res {
        Err(e) if device && e.raw_os_error() == Some(libc::EBUSY) => {
            bail!("{} is in use; {in_use_hint}", p.display())
        }
        res => res.with_context(|| format!("couldn't open {}", p.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_nodes_are_devices() {
        assert!(is_device(Path::new("/dev/null")).unwrap());
    }
}
