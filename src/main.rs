use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam::thread::scope;
use fs2;
use humantime::{format_duration, FormattedDuration};
use indicatif::ProgressBar;
use indicatif::{MultiProgress, ProgressStyle};
use rand::prelude::*;
use rand::thread_rng;
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(name = "rand_wipe", about = "Writes random data to specified paths")]
struct Opt {
    #[structopt(parse(from_os_str))]
    paths: Vec<PathBuf>,
}

fn to_dur(start: Instant) -> FormattedDuration {
    let el = start.elapsed();
    format_duration(
        Duration::from_secs(el.as_secs()) + Duration::from_millis(el.subsec_millis() as u64),
    )
}

fn main() {
    let opt = Opt::from_args();

    scope(|s| {
        let multi = MultiProgress::new();

        for p in opt.paths.into_iter() {
            let prog_bar = ProgressBar::new(fs2::free_space(&p).expect(&format!("Couldn't get the total space for {}", p.display())));

            prog_bar.set_style(ProgressStyle::default_bar().template(
                "[{elapsed_precise}] {bar:40.cyan/blue} {bytes:>7}/{total_bytes:7} => {bytes_per_sec} :: {eta_precise} {msg}",
            ));
            multi.add(prog_bar.clone());

            let mut fh = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&p).expect(&format!("Couldn't open {}", p.display()));

            prog_bar.set_message(&format!("{}", p.display()));
            s.spawn(move |_| {
                let start = Instant::now();

                let rng = thread_rng();
                let mut chacha = rand_chacha::ChaCha12Rng::from_rng(rng).unwrap();
                let mut buf = [0u8; 1 << 19];

                loop {
                    chacha.fill_bytes(&mut buf);
                    match fh.write(&mut buf) {
                        Ok(l) => prog_bar.inc(l as u64),
                        Err(e) => {
                            prog_bar.println(&format!("Error writing to {}: {}", p.display(), e));
                            break;
                        }
                    }
                }

                prog_bar.println(format!(
                    "Finished {} after {}",
                    p.display(),
                    to_dur(start),
                ));

                prog_bar.finish_and_clear();
            });
        }

        multi.join().unwrap();
    }
    ).unwrap();
}
