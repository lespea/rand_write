use std::fs::{File, OpenOptions};
use std::io::{BufRead, ErrorKind, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::thread::scope;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use humantime::{FormattedDuration, format_duration};
use indicatif::{HumanBytes, MultiProgress, ProgressBar, ProgressStyle};
use rand::prelude::*;
use rand_chacha::ChaCha8Rng;

#[derive(Parser)]
#[clap(name = "rand_wipe", about = "Writes random data to specified paths")]
struct Opt {
    /// Don't ask for confirmation before wiping
    #[arg(short, long)]
    yes: bool,

    /// Keep file targets afterwards instead of deleting them
    #[arg(short, long)]
    keep: bool,

    /// Devices to overwrite entirely, or files to fill their filesystem's free space with
    #[arg(required = true)]
    paths: Vec<PathBuf>,
}

/// Whether the path names a disk rather than a (possibly not yet existing) regular file
#[cfg(unix)]
fn is_device(p: &Path) -> Result<bool> {
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

/// Whether the path names a disk rather than a (possibly not yet existing) regular file
#[cfg(windows)]
fn is_device(p: &Path) -> Result<bool> {
    Ok(p.as_os_str().to_string_lossy().starts_with(r"\\.\"))
}

#[cfg(target_os = "linux")]
const IN_USE_HINT: &str =
    "unmount it and its partitions, and stop anything else holding it (swap, LVM, dm-crypt, RAID)";
#[cfg(target_os = "macos")]
const IN_USE_HINT: &str = "unmount it first with `diskutil unmountDisk`";
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
const IN_USE_HINT: &str = "unmount it first";

/// Opens with `uncached`, falling back to `opt` when the target rejects it with `invalid`
#[cfg(any(target_os = "linux", windows))]
fn open_fallback(
    uncached: &OpenOptions,
    opt: &OpenOptions,
    p: &Path,
    invalid: i32,
    what: &str,
) -> std::io::Result<File> {
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

/// Opens a target bypassing the page cache where possible. Devices must already exist and not be
/// in use; files are created if missing.
fn open(p: &Path, device: bool) -> Result<File> {
    let mut opt = OpenOptions::new();
    opt.write(true).create(!device);

    #[cfg(target_os = "linux")]
    let res = {
        use std::os::unix::fs::OpenOptionsExt;

        // On a block device O_EXCL fails with EBUSY while it or any of its partitions is mounted
        // or otherwise claimed
        let excl = if device { libc::O_EXCL } else { 0 };
        opt.custom_flags(excl);
        open_fallback(
            opt.clone().custom_flags(excl | libc::O_DIRECT),
            &opt,
            p,
            libc::EINVAL,
            "O_DIRECT",
        )
    };

    #[cfg(windows)]
    let res = {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH,
        };

        // IOCTL_DISK_GET_LENGTH_INFO needs read access
        opt.read(true);
        open_fallback(
            opt.clone()
                .custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH),
            &opt,
            p,
            ERROR_INVALID_PARAMETER as i32,
            "unbuffered I/O",
        )
    };

    #[cfg(not(any(target_os = "linux", windows)))]
    let res = opt.open(p);

    let fh = match res {
        #[cfg(unix)]
        Err(e) if device && e.raw_os_error() == Some(libc::EBUSY) => {
            bail!("{} is in use; {IN_USE_HINT}", p.display())
        }
        res => res.with_context(|| format!("couldn't open {}", p.display()))?,
    };

    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;

        // SAFETY: plain fcntl on a descriptor we own
        if unsafe { libc::fcntl(fh.as_raw_fd(), libc::F_NOCACHE, 1) } == -1 {
            println!(
                "{} doesn't support F_NOCACHE; falling back to buffered writes ({})",
                p.display(),
                std::io::Error::last_os_error()
            );
        }
    }

    #[cfg(windows)]
    if device {
        check_no_volumes(&fh, p)?;
    }

    Ok(fh)
}

/// DeviceIoControl with no input and a fixed-size output
#[cfg(windows)]
fn ioctl_out<T>(fh: &File, code: u32, out: &mut T) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;

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
#[cfg(windows)]
fn check_no_volumes(fh: &File, p: &Path) -> Result<()> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FindFirstVolumeW, FindNextVolumeW, FindVolumeClose,
    };
    use windows_sys::Win32::System::Ioctl::{
        IOCTL_STORAGE_GET_DEVICE_NUMBER, STORAGE_DEVICE_NUMBER,
    };

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
#[cfg(windows)]
fn volume_disks(vol: &str) -> std::io::Result<Vec<u32>> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS;
    use windows_sys::Win32::System::Ioctl::DISK_EXTENT;

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

/// Exact size of a disk
#[cfg(not(any(target_os = "macos", windows)))]
fn device_size(fh: &mut File) -> std::io::Result<u64> {
    use std::io::{Seek, SeekFrom};

    let size = fh.seek(SeekFrom::End(0))?;
    fh.rewind()?;
    Ok(size)
}

/// Exact size of a disk
#[cfg(target_os = "macos")]
fn device_size(fh: &mut File) -> std::io::Result<u64> {
    use std::os::fd::AsRawFd;

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

/// Exact size of a disk
#[cfg(windows)]
fn device_size(fh: &mut File) -> std::io::Result<u64> {
    use windows_sys::Win32::System::Ioctl::{GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO};

    let mut info = GET_LENGTH_INFORMATION::default();
    ioctl_out(fh, IOCTL_DISK_GET_LENGTH_INFO, &mut info)?;
    Ok(info.Length as u64)
}

/// Space left for a regular file to grow into
#[cfg(unix)]
fn free_space(fh: &File, _p: &Path) -> std::io::Result<u64> {
    use std::os::fd::AsRawFd;

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

/// Space left for a regular file to grow into
#[cfg(windows)]
fn free_space(_fh: &File, p: &Path) -> std::io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

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

fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn confirm() -> Result<bool> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        bail!("no terminal to confirm on; pass --yes to wipe anyway");
    }
    print!("Continue? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    stdin.lock().read_line(&mut answer)?;
    Ok(is_yes(&answer))
}

fn to_dur(start: Instant) -> FormattedDuration {
    let el = start.elapsed();
    format_duration(
        Duration::from_secs(el.as_secs()) + Duration::from_millis(el.subsec_millis() as u64),
    )
}

// Larger O_DIRECT writes get split into more concurrent requests, i.e. a deeper effective queue
const BUF_SIZE: usize = 4 << 20;
// Buffers cycling between the RNG thread and the writer thread
const NUM_BUFS: usize = 4;

// O_DIRECT and FILE_FLAG_NO_BUFFERING need at most 4K; F_NOCACHE wants page alignment,
// which is 16K on Apple Silicon
#[cfg_attr(target_os = "macos", repr(align(16384)))]
#[cfg_attr(not(target_os = "macos"), repr(align(4096)))]
struct Buf([u8; BUF_SIZE]);

// Every full write must stay a multiple of the alignment
const _: () = assert!(BUF_SIZE.is_multiple_of(align_of::<Buf>()));

impl Buf {
    #[inline]
    fn new() -> Box<Self> {
        // SAFETY: all-zero bytes are a valid `[u8; N]`
        unsafe { Box::new_zeroed().assume_init() }
    }
}

/// How much of the next buffer to write: devices stop at exactly their size, files write whole
/// buffers until the disk fills up. Device sizes are sector multiples, so a trimmed last chunk
/// stays aligned.
fn chunk_len(is_device: bool, left: u64) -> usize {
    if is_device {
        left.min(BUF_SIZE as u64) as usize
    } else {
        BUF_SIZE
    }
}

struct Target {
    path: PathBuf,
    fh: File,
    size: u64,
    // Devices get written to exactly their size; files until the disk is full
    is_device: bool,
    rng: ChaCha8Rng,
}

fn main() -> Result<()> {
    let Opt { yes, keep, paths } = Opt::parse();

    // Devices are opened up front so in-use ones are refused and the prompt can show their sizes;
    // files aren't touched until confirmed
    let planned = paths
        .into_iter()
        .map(|path| {
            if !is_device(&path)? {
                return Ok((path, None));
            }
            let mut fh = open(&path, true)?;
            let size = device_size(&mut fh)
                .with_context(|| format!("couldn't get the size of {}", path.display()))?;
            Ok((path, Some((fh, size))))
        })
        .collect::<Result<Vec<_>>>()?;

    if !yes {
        println!("This will overwrite:");
        for (path, dev) in &planned {
            match dev {
                Some((_, size)) => {
                    println!(
                        "  {}: the entire device ({})",
                        path.display(),
                        HumanBytes(*size)
                    )
                }
                None => println!(
                    "  {}: a file filling its filesystem's free space{}",
                    path.display(),
                    if keep { "" } else { ", deleted afterwards" }
                ),
            }
        }
        if !confirm()? {
            bail!("aborted");
        }
    }

    // Set everything up before spawning, so one bad path doesn't leave the others half-wiped
    let targets = planned
        .into_iter()
        .map(|(path, dev)| {
            let (fh, size, is_device) = match dev {
                Some((fh, size)) => (fh, size, true),
                None => {
                    let fh = open(&path, false)?;
                    fh.set_len(0)
                        .with_context(|| format!("couldn't truncate {}", path.display()))?;
                    let free = free_space(&fh, &path).with_context(|| {
                        format!("couldn't get the free space for {}", path.display())
                    })?;
                    (fh, free, false)
                }
            };
            let rng = ChaCha8Rng::try_from_rng(&mut rand::rngs::SysRng)
                .context("failed to seed RNG from OS")?;
            Ok(Target {
                path,
                fh,
                size,
                is_device,
                rng,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let multi = MultiProgress::new();
    // Kept narrow and the path on its own line: once a line wraps, shrinking the terminal leaves
    // stale copies of the bar behind
    let sty = ProgressStyle::default_bar().template(
        "{msg}\n[{elapsed_precise}] {bar:24.cyan/blue} {bytes:>9}/{total_bytes:<9} {bytes_per_sec:>11} eta {eta}",
    )?;

    scope(|s| {
        for Target {
            path: p,
            mut fh,
            size,
            is_device,
            mut rng,
        } in targets
        {
            let prog_bar = multi.add(ProgressBar::new(size));
            prog_bar.set_style(sty.clone());
            prog_bar.set_message(format!("{}", p.display()));

            // Buffers go empty -> RNG thread -> full -> writer thread -> empty, so filling
            // overlaps with writing. When the writer stops, both channels hang up and the
            // RNG thread exits on its own.
            let (empty_tx, empty_rx) = sync_channel::<Box<Buf>>(NUM_BUFS);
            let (full_tx, full_rx) = sync_channel::<Box<Buf>>(NUM_BUFS);
            for _ in 0..NUM_BUFS {
                empty_tx.send(Buf::new()).expect("receiver is alive");
            }

            s.spawn(move || {
                for mut buf in empty_rx {
                    rng.fill_bytes(&mut buf.0);
                    if full_tx.send(buf).is_err() {
                        break;
                    }
                }
            });

            s.spawn(move || {
                let start = Instant::now();
                let mut left = size;

                'outer: for buf in full_rx.iter() {
                    let len = chunk_len(is_device, left);
                    if len == 0 {
                        break;
                    }

                    loop {
                        match fh.write(&buf.0[..len]) {
                            Ok(0) => break 'outer,
                            Ok(l) => {
                                prog_bar.inc(l as u64);
                                left = left.saturating_sub(l as u64);
                                break;
                            }
                            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                            Err(e) => {
                                if e.kind() != ErrorKind::StorageFull {
                                    prog_bar.println(format!(
                                        "Error writing to {}: {}",
                                        p.display(),
                                        e
                                    ));
                                }
                                break 'outer;
                            }
                        }
                    }
                    if empty_tx.send(buf).is_err() {
                        break;
                    }
                }
                drop(empty_tx);

                if let Err(e) = fh.sync_all() {
                    prog_bar.println(format!("Error syncing {}: {}", p.display(), e));
                }
                drop(fh);

                if !is_device
                    && !keep
                    && let Err(e) = std::fs::remove_file(&p)
                {
                    prog_bar.println(format!("Error removing {}: {}", p.display(), e));
                }

                prog_bar.println(format!("Finished {} after {}", p.display(), to_dur(start)));
                prog_bar.finish_and_clear();
            });
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path in the temp dir that's removed on drop
    struct TempFile(PathBuf);

    impl TempFile {
        fn missing(name: &str) -> Self {
            Self(std::env::temp_dir().join(format!("rand_wipe-{}-{name}", std::process::id())))
        }

        fn new(name: &str) -> Self {
            let tmp = Self::missing(name);
            File::create(&tmp.0).expect("create temp file");
            tmp
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn regular_files_are_not_devices() {
        assert!(!is_device(&TempFile::new("device").0).unwrap());
        assert!(!is_device(&TempFile::missing("device-missing").0).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn device_nodes_are_devices() {
        assert!(is_device(Path::new("/dev/null")).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn device_paths_are_devices() {
        assert!(is_device(Path::new(r"\\.\PhysicalDrive0")).unwrap());
        assert!(!is_device(Path::new(r"C:\wipe.bin")).unwrap());
    }

    #[test]
    fn open_creates_missing_files() {
        let tmp = TempFile::missing("create");
        open(&tmp.0, false).unwrap();
        assert!(tmp.0.is_file());
    }

    #[test]
    fn free_space_is_reported() {
        let tmp = TempFile::new("free");
        let fh = open(&tmp.0, false).unwrap();
        assert!(free_space(&fh, &tmp.0).unwrap() > 0);
    }

    #[test]
    fn free_space_of_bare_file_name() {
        // No parent directory, so Windows has to fall back to "."
        let name = format!("rand_wipe-{}-bare", std::process::id());
        let tmp = TempFile::new("bare");
        let fh = open(&tmp.0, false).unwrap();
        assert!(free_space(&fh, Path::new(&name)).unwrap() > 0);
    }

    #[test]
    fn aligned_write_is_accepted() {
        let tmp = TempFile::new("write");
        let mut fh = open(&tmp.0, false).unwrap();
        let buf = Buf::new();
        assert_eq!(fh.write(&buf.0).unwrap(), BUF_SIZE);
        fh.sync_all().unwrap();
        assert_eq!(fh.metadata().unwrap().len(), BUF_SIZE as u64);
    }

    #[test]
    fn device_writes_stop_at_its_size() {
        let size = 2 * BUF_SIZE as u64 + 512 * 3;
        let mut left = size;
        let mut chunks = vec![];
        loop {
            let len = chunk_len(true, left);
            if len == 0 {
                break;
            }
            chunks.push(len);
            left -= len as u64;
        }
        assert_eq!(chunks, [BUF_SIZE, BUF_SIZE, 512 * 3]);
    }

    #[test]
    fn file_writes_are_always_full_buffers() {
        assert_eq!(chunk_len(false, 0), BUF_SIZE);
        assert_eq!(chunk_len(false, 512), BUF_SIZE);
    }

    #[test]
    fn only_yes_confirms() {
        for yes in ["y", "Y", "yes", " YES \n"] {
            assert!(is_yes(yes), "{yes:?}");
        }
        for no in ["", "\n", "n", "no", "yep", "maybe"] {
            assert!(!is_yes(no), "{no:?}");
        }
    }
}
