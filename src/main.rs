use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Error, Result};
use clap::Parser;
use humantime::{format_duration, FormattedDuration};
use indicatif::ProgressBar;
use indicatif::{MultiProgress, ProgressStyle};
use rand::prelude::*;
use rand::thread_rng;
use std::thread::scope;

#[derive(Debug, Parser)]
#[clap(name = "rand_wipe", about = "Writes random data to specified paths")]
struct Opt {
    paths: Vec<PathBuf>,
}

#[cfg(target_os = "linux")]
fn freespace(p: &Path) -> u64 {
    fs2::free_space(p).unwrap_or_else(|_| {
        Command::new("blockdev")
            .arg("--getsize64")
            .arg(p.as_os_str())
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(Error::new)
            .and_then(|o| String::from_utf8(o.stdout).context(""))
            .and_then(|o| str::parse::<u64>(o.trim()).context(""))
            .unwrap_or_else(|_| panic!("Couldn't get the total space for {}", p.display()))
    })
}

#[cfg(target_os = "windows")]
fn freespace(path: &Path) -> u64 {
    fs2::free_space(&p).expect(&format!("Couldn't get the total space for {}", p.display()))
}

#[cfg(target_os = "linux")]
fn open(p: &Path) -> File {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .custom_flags(libc::O_DIRECT)
        .open(p)
        .unwrap_or_else(|_| panic!("Couldn't open {}", p.display()))
}

#[cfg(target_os = "windows")]
fn open(path: &Path) -> File {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&p)
        .unwrap_or_else(|_| panic!("Couldn't open {}", p.display()))
}

fn to_dur(start: Instant) -> FormattedDuration {
    let el = start.elapsed();
    format_duration(
        Duration::from_secs(el.as_secs()) + Duration::from_millis(el.subsec_millis() as u64),
    )
}

const BUF_SIZE: usize = 2 << 20;

#[derive(Clone, Copy)]
#[repr(align(8192))]
struct Buf([u8; BUF_SIZE]);

impl Buf {
    #[inline]
    fn new() -> Self {
        Buf([0u8; BUF_SIZE])
    }
}

fn main() -> Result<()> {
    let opt = Opt::parse();

    if opt.paths.is_empty() {
        return Err(anyhow!("Must provide at least one drive to wipe"));
    }

    scope(|s| {
        let multi = MultiProgress::new();

        let sty = ProgressStyle::default_bar().template(
                "[{elapsed_precise}] {bar:40.cyan/blue} {bytes:>7}/{total_bytes:7} => {bytes_per_sec} :: {eta_precise} {msg}",
            ).unwrap();

        for p in opt.paths {
            let mut fh = open(&p);

            let prog_bar = ProgressBar::new(freespace(&p));

            prog_bar.set_style(sty.clone());
            multi.add(prog_bar.clone());

            prog_bar.set_message(format!("{}", p.display()));
            s.spawn(move || {
                let mut chacha = rand_chacha::ChaCha12Rng::from_rng(thread_rng()).unwrap();
                let mut buf = Buf::new();

                let start = Instant::now();
                loop {
                    chacha.fill_bytes(&mut buf.0);
                    match fh.write(&buf.0) {
                        Ok(l) => prog_bar.inc(l as u64),
                        Err(e) => {
                            if !e.to_string().contains("No space left") {
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

                prog_bar.println(format!("Finished {} after {}", p.display(), to_dur(start)));
                prog_bar.finish_and_clear();
            });
        }
    });

    Ok(())
}
