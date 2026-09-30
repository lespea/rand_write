use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::thread::scope;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use humantime::{FormattedDuration, format_duration};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rand::prelude::*;
use rand_chacha::ChaCha8Rng;

#[derive(Parser)]
#[clap(name = "rand_wipe", about = "Writes random data to specified paths")]
struct Opt {
    #[arg(required = true)]
    paths: Vec<PathBuf>,
}

fn open(p: &Path) -> Result<File> {
    let mut opt = OpenOptions::new();
    opt.write(true);

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;

        match opt.clone().custom_flags(libc::O_DIRECT).open(p) {
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => println!(
                "{} doesn't support O_DIRECT; falling back to buffered writes",
                p.display()
            ),
            res => return res.with_context(|| format!("couldn't open {}", p.display())),
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH,
        };

        // IOCTL_DISK_GET_LENGTH_INFO needs read access
        opt.read(true);
        match opt
            .clone()
            .custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH)
            .open(p)
        {
            Err(e) if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) => println!(
                "{} doesn't support unbuffered I/O; falling back to buffered writes",
                p.display()
            ),
            res => return res.with_context(|| format!("couldn't open {}", p.display())),
        }
    }

    let fh = opt
        .open(p)
        .with_context(|| format!("couldn't open {}", p.display()))?;

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

    Ok(fh)
}

/// Exact size of a block/raw device, or `None` for a regular file
#[cfg(target_os = "linux")]
fn device_size(fh: &mut File) -> std::io::Result<Option<u64>> {
    use std::io::{Seek, SeekFrom};
    use std::os::unix::fs::FileTypeExt;

    if !fh.metadata()?.file_type().is_block_device() {
        return Ok(None);
    }
    let size = fh.seek(SeekFrom::End(0))?;
    fh.rewind()?;
    Ok(Some(size))
}

/// Exact size of a block/raw device, or `None` for a regular file
#[cfg(target_os = "macos")]
fn device_size(fh: &mut File) -> std::io::Result<Option<u64>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileTypeExt;

    // _IOR('d', 24, uint32_t) and _IOR('d', 25, uint64_t) from <sys/disk.h>
    const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6418;
    const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x4008_6419;

    let ft = fh.metadata()?.file_type();
    if !(ft.is_block_device() || ft.is_char_device()) {
        return Ok(None);
    }

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
    Ok(Some(u64::from(block_size) * block_count))
}

/// Exact size of a disk/volume, or `None` for a regular file
#[cfg(windows)]
fn device_size(fh: &mut File) -> std::io::Result<Option<u64>> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO};

    let mut info = GET_LENGTH_INFORMATION::default();
    let mut returned = 0;
    // SAFETY: the output buffer is a GET_LENGTH_INFORMATION of the size passed in
    let ok = unsafe {
        DeviceIoControl(
            fh.as_raw_handle(),
            IOCTL_DISK_GET_LENGTH_INFO,
            std::ptr::null(),
            0,
            (&raw mut info).cast(),
            size_of::<GET_LENGTH_INFORMATION>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    // Regular files don't answer disk ioctls
    Ok((ok != 0).then_some(info.Length as u64))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn device_size(_fh: &mut File) -> std::io::Result<Option<u64>> {
    Ok(None)
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
    let opt = Opt::parse();

    // Set everything up before spawning, so one bad path doesn't leave the others half-wiped
    let targets = opt
        .paths
        .into_iter()
        .map(|path| {
            let mut fh = open(&path)?;
            let (size, is_device) = match device_size(&mut fh)
                .with_context(|| format!("couldn't get the size of {}", path.display()))?
            {
                Some(size) => (size, true),
                None => {
                    fh.set_len(0)
                        .with_context(|| format!("couldn't truncate {}", path.display()))?;
                    let free = free_space(&fh, &path).with_context(|| {
                        format!("couldn't get the free space for {}", path.display())
                    })?;
                    (free, false)
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
    let sty = ProgressStyle::default_bar().template(
        "[{elapsed_precise}] {bar:40.cyan/blue} {bytes:>7}/{total_bytes:7} => {bytes_per_sec} :: {eta_precise} {msg}",
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

    /// A file in the temp dir that's removed on drop
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("rand_wipe-{}-{name}", std::process::id()));
            File::create(&path).expect("create temp file");
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn regular_file_is_not_a_device() {
        let tmp = TempFile::new("device");
        let mut fh = open(&tmp.0).unwrap();
        assert_eq!(device_size(&mut fh).unwrap(), None);
    }

    #[test]
    fn free_space_is_reported() {
        let tmp = TempFile::new("free");
        let fh = open(&tmp.0).unwrap();
        assert!(free_space(&fh, &tmp.0).unwrap() > 0);
    }

    #[test]
    fn free_space_of_bare_file_name() {
        // No parent directory, so Windows has to fall back to "."
        let name = format!("rand_wipe-{}-bare", std::process::id());
        let tmp = TempFile::new("bare");
        let fh = open(&tmp.0).unwrap();
        assert!(free_space(&fh, Path::new(&name)).unwrap() > 0);
    }

    #[test]
    fn aligned_write_is_accepted() {
        let tmp = TempFile::new("write");
        let mut fh = open(&tmp.0).unwrap();
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
}
