use std::fs::File;
use std::io::{BufRead, ErrorKind, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::mpsc::sync_channel;
use std::thread::scope;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use humantime::{FormattedDuration, format_duration};
use indicatif::{HumanBytes, MultiProgress, ProgressBar, ProgressStyle};
use rand::prelude::*;
use rand_chacha::ChaCha8Rng;

mod platform;

use platform::{device_size, free_space, is_device, open};

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
    use std::path::Path;

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
