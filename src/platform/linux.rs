use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::Result;

use super::open_fallback;
use super::unix::check_open;
pub use super::unix::{free_space, is_device};

pub fn open(p: &Path, device: bool) -> Result<File> {
    let mut opt = OpenOptions::new();
    opt.write(true).create(!device);

    // On a block device O_EXCL fails with EBUSY while it or any of its partitions is mounted or
    // otherwise claimed
    let excl = if device { libc::O_EXCL } else { 0 };
    opt.custom_flags(excl);
    let res = open_fallback(
        opt.clone().custom_flags(excl | libc::O_DIRECT),
        &opt,
        p,
        libc::EINVAL,
        "O_DIRECT",
    );

    check_open(
        res,
        p,
        device,
        "unmount it and its partitions, and stop anything else holding it (swap, LVM, dm-crypt, RAID)",
    )
}

pub fn device_size(fh: &mut File) -> std::io::Result<u64> {
    let size = fh.seek(SeekFrom::End(0))?;
    fh.rewind()?;
    Ok(size)
}
