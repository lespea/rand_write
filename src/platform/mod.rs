//! Everything that differs per OS. Each platform module provides:
//!
//! - `is_device(&Path) -> Result<bool>`: whether a path names a disk rather than a (possibly not
//!   yet existing) regular file
//! - `open(&Path, device: bool) -> Result<File>`: opens bypassing the page cache where possible;
//!   devices must already exist and not be in use, files are created if missing
//! - `device_size(&mut File) -> io::Result<u64>`: exact size of a disk
//! - `free_space(&File, &Path) -> io::Result<u64>`: space left for a regular file to grow into

#[cfg(unix)]
mod unix;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
compile_error!("only Linux, macOS and Windows are supported");

/// Opens with `uncached`, falling back to `opt` when the target rejects it with `invalid`
#[cfg(any(target_os = "linux", windows))]
fn open_fallback(
    uncached: &std::fs::OpenOptions,
    opt: &std::fs::OpenOptions,
    p: &std::path::Path,
    invalid: i32,
    what: &str,
) -> std::io::Result<std::fs::File> {
    match uncached.open(p) {
        Err(e) if e.raw_os_error() == Some(invalid) => {
            println!(
                "{} doesn't support {what}; falling back to buffered writes",
                p.display()
            );
            opt.open(p)
        }
        res => res,
    }
}
