use std::fs::{File, OpenOptions};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH, FindFirstVolumeW, FindNextVolumeW,
    FindVolumeClose, GetDiskFreeSpaceExW, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DISK_EXTENT, GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO,
    IOCTL_STORAGE_GET_DEVICE_NUMBER, STORAGE_DEVICE_NUMBER,
};

use super::open_fallback;

pub fn is_device(p: &Path) -> Result<bool> {
    Ok(p.as_os_str().to_string_lossy().starts_with(r"\\.\"))
}

pub fn open(p: &Path, device: bool) -> Result<File> {
    let mut opt = OpenOptions::new();
    // IOCTL_DISK_GET_LENGTH_INFO needs read access
    opt.read(true).write(true).create(!device);

    let fh = open_fallback(
        opt.clone()
            .custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH),
        &opt,
        p,
        ERROR_INVALID_PARAMETER as i32,
        "unbuffered I/O",
    )
    .with_context(|| format!("couldn't open {}", p.display()))?;

    if device {
        check_no_volumes(&fh, p)?;
    }
    Ok(fh)
}

pub fn device_size(fh: &mut File) -> std::io::Result<u64> {
    let mut info = GET_LENGTH_INFORMATION::default();
    ioctl_out(fh, IOCTL_DISK_GET_LENGTH_INFO, &mut info)?;
    Ok(info.Length as u64)
}

pub fn free_space(_fh: &File, p: &Path) -> std::io::Result<u64> {
    // It wants a directory, not the file itself
    let dir = match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let mut avail = 0;
    // SAFETY: `wide` is NUL-terminated and the unused outputs may be null
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut avail,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(avail)
}

/// DeviceIoControl with no input and a fixed-size output
fn ioctl_out<T>(fh: &File, code: u32, out: &mut T) -> std::io::Result<()> {
    let mut returned = 0;
    // SAFETY: `out` is a writable T of the size passed in
    let ok = unsafe {
        DeviceIoControl(
            fh.as_raw_handle(),
            code,
            std::ptr::null(),
            0,
            (out as *mut T).cast(),
            size_of::<T>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Windows blocks writes to any sector a mounted volume owns, so only whole disks with no volumes
/// left on them can be wiped
fn check_no_volumes(fh: &File, p: &Path) -> Result<()> {
    let mut num = STORAGE_DEVICE_NUMBER::default();
    ioctl_out(fh, IOCTL_STORAGE_GET_DEVICE_NUMBER, &mut num)
        .with_context(|| format!("couldn't get the disk number of {}", p.display()))?;
    if num.PartitionNumber != 0 {
        bail!(
            r"{} is a partition or volume; target the whole disk (\\.\PhysicalDriveN) instead",
            p.display()
        );
    }

    let mut on_disk = vec![];
    let mut name = [0u16; 261];
    // SAFETY: the length passed matches `name`
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if find == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).context("couldn't list volumes");
    }
    loop {
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let vol = String::from_utf16_lossy(&name[..len]);
        if volume_disks(&vol).is_ok_and(|disks| disks.contains(&num.DeviceNumber)) {
            on_disk.push(vol);
        }
        // SAFETY: as above; fails with ERROR_NO_MORE_FILES after the last volume
        if unsafe { FindNextVolumeW(find, name.as_mut_ptr(), name.len() as u32) } == 0 {
            break;
        }
    }
    // SAFETY: `find` came from FindFirstVolumeW and isn't used afterwards
    unsafe { FindVolumeClose(find) };

    if !on_disk.is_empty() {
        bail!(
            "{} still has volumes on it ({}); take it offline or `clean` it in diskpart first",
            p.display(),
            on_disk.join(", ")
        );
    }
    Ok(())
}

/// Disk numbers a volume (`\\?\Volume{...}\`) lives on
fn volume_disks(vol: &str) -> std::io::Result<Vec<u32>> {
    // VOLUME_DISK_EXTENTS with room for volumes spanning several disks
    #[repr(C)]
    struct Extents {
        count: u32,
        extents: [DISK_EXTENT; 32],
    }

    // The volume device is the name without its trailing backslash, and querying it needs no
    // access rights
    let fh = OpenOptions::new()
        .access_mode(0)
        .open(vol.trim_end_matches('\\'))?;
    // SAFETY: all-zero is a valid `Extents`
    let mut ext: Extents = unsafe { std::mem::zeroed() };
    ioctl_out(&fh, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &mut ext)?;
    let count = (ext.count as usize).min(ext.extents.len());
    Ok(ext.extents[..count].iter().map(|e| e.DiskNumber).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_paths_are_devices() {
        assert!(is_device(Path::new(r"\\.\PhysicalDrive0")).unwrap());
        assert!(!is_device(Path::new(r"C:\wipe.bin")).unwrap());
    }
}
