use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::Result;

use super::unix::check_open;
pub use super::unix::{free_space, is_device};

pub fn open(p: &Path, device: bool) -> Result<File> {
    // Opening a disk with any of its volumes mounted fails with EBUSY
    let fh = check_open(
        OpenOptions::new().write(true).create(!device).open(p),
        p,
        device,
        "unmount it first with `diskutil unmountDisk`",
    )?;

    // SAFETY: plain fcntl on a descriptor we own
    if unsafe { libc::fcntl(fh.as_raw_fd(), libc::F_NOCACHE, 1) } == -1 {
        println!(
            "{} doesn't support F_NOCACHE; falling back to buffered writes ({})",
            p.display(),
            std::io::Error::last_os_error()
        );
    }

    Ok(fh)
}

pub fn device_size(fh: &mut File) -> std::io::Result<u64> {
    // _IOR('d', 24, uint32_t) and _IOR('d', 25, uint64_t) from <sys/disk.h>
    const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6418;
    const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x4008_6419;

    let fd = fh.as_raw_fd();
    let (mut block_size, mut block_count) = (0u32, 0u64);
    // SAFETY: each ioctl writes a single value of the pointed-to type
    unsafe {
        if libc::ioctl(fd, DKIOCGETBLOCKSIZE, &raw mut block_size) == -1
            || libc::ioctl(fd, DKIOCGETBLOCKCOUNT, &raw mut block_count) == -1
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(u64::from(block_size) * block_count)
}
