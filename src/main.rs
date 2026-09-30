use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::thread::scope;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use humantime::{FormattedDuration, format_duration};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rand::prelude::*;
use rand_chacha::ChaCha12Rng;

#[derive(Parser)]
#[clap(name = "rand_wipe", about = "Writes random data to specified paths")]
struct Opt {
    #[arg(required = true)]
    paths: Vec<PathBuf>,
}

#[cfg(target_os = "linux")]
fn blockdev_size(p: &Path) -> Result<u64> {
    use std::process::{Command, Stdio};

    let out = Command::new("blockdev")
        .arg("--getsize64")
        .arg(p.as_os_str())
        .stdin(Stdio::null())
        .output()
        .context("couldn't run blockdev")?;

    if !out.status.success() {
        anyhow::bail!(
            "blockdev failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let size = String::from_utf8(out.stdout).context("blockdev output isn't UTF-8")?;
    let size = size.trim();
    size.parse()
        .with_context(|| format!("invalid disk size from blockdev: {size:?}"))
}

fn freespace(p: &Path) -> Result<u64> {
    #[cfg(target_os = "linux")]
    match blockdev_size(p) {
        Ok(size) => return Ok(size),
        Err(err) => println!(
            "Couldn't get the device size of {}; falling back to fs2 ({err:#})",
            p.display()
        ),
    }

    fs2::free_space(p).with_context(|| format!("couldn't get the total space for {}", p.display()))
}

fn open(p: &Path) -> Result<File> {
    let mut opt = OpenOptions::new();
    opt.write(true).truncate(true);

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

    opt.open(p)
        .with_context(|| format!("couldn't open {}", p.display()))
}

fn to_dur(start: Instant) -> FormattedDuration {
    let el = start.elapsed();
    format_duration(
        Duration::from_secs(el.as_secs()) + Duration::from_millis(el.subsec_millis() as u64),
    )
}

const BUF_SIZE: usize = 1 << 20;

#[repr(align(8192))]
struct Buf([u8; BUF_SIZE]);

impl Buf {
    #[inline]
    fn new() -> Box<Self> {
        // SAFETY: all-zero bytes are a valid `[u8; N]`
        unsafe { Box::new_zeroed().assume_init() }
    }
}

struct Target {
    path: PathBuf,
    fh: File,
    size: u64,
    rng: ChaCha12Rng,
}

fn main() -> Result<()> {
    let opt = Opt::parse();

    // Set everything up before spawning, so one bad path doesn't leave the others half-wiped
    let targets = opt
        .paths
        .into_iter()
        .map(|path| {
            let fh = open(&path)?;
            let size = freespace(&path)?;
            let rng = ChaCha12Rng::try_from_rng(&mut rand::rngs::SysRng)
                .context("failed to seed RNG from OS")?;
            Ok(Target {
                path,
                fh,
                size,
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
            mut rng,
        } in targets
        {
            let prog_bar = multi.add(ProgressBar::new(size));
            prog_bar.set_style(sty.clone());
            prog_bar.set_message(format!("{}", p.display()));

            s.spawn(move || {
                let start = Instant::now();

                let mut buf = Buf::new();
                loop {
                    rng.fill_bytes(&mut buf.0);
                    match fh.write(&buf.0) {
                        Ok(l) => prog_bar.inc(l as u64),
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(e) => {
                            if e.kind() != ErrorKind::StorageFull {
                                prog_bar.println(format!(
                                    "Error writing to {}: {}",
                                    p.display(),
                                    e
                                ));
                            }
                            break;
                        }
                    }
                }

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
