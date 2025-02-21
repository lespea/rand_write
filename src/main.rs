use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::scope;
use std::time::{Duration, Instant};

use anyhow::{Context, Error, Result};
use clap::Parser;
use humantime::{format_duration, FormattedDuration};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rand::prelude::*;

#[derive(Parser)]
#[clap(name = "rand_wipe", about = "Writes random data to specified paths")]
struct Opt {
    #[arg(required = true)]
    paths: Vec<PathBuf>,
}

#[cfg(target_os = "linux")]
fn freespace(p: &Path) -> u64 {
    Command::new("blockdev")
        .arg("--getsize64")
        .arg(p.as_os_str())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(Error::new)
        .and_then(|o| String::from_utf8(o.stdout).context(""))
        .and_then(|o| {
            o.trim()
                .parse()
                .context(format!("Invalid disk size number? {o}"))
        })
        .unwrap_or_else(|err| {
            println!("Error getting size the standard way; falling back to fs2 ({err})");
            fs2::free_space(p).unwrap_or_else(|err| {
                panic!("Couldn't get the total space for {} ({err})", p.display())
            })
        })
}

#[cfg(target_os = "windows")]
fn freespace(p: &Path) -> u64 {
    fs2::free_space(p).expect(&format!("Couldn't get the total space for {}", p.display()))
}

fn open(p: &Path) -> File {
    let mut opt = OpenOptions::new();
    opt.create(true).write(true).truncate(true);

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opt.custom_flags(libc::O_DIRECT);
    }

    opt.open(p)
        .unwrap_or_else(|err| panic!("Couldn't open {} ({err})", p.display()))
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
    fn new() -> Self {
        Buf([0u8; BUF_SIZE])
    }
}

fn main() -> Result<()> {
    let opt = Opt::parse();

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
                let start = Instant::now();
                let mut chacha = rand_chacha::ChaCha12Rng::from_os_rng();

                let mut buf = Buf::new();
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
