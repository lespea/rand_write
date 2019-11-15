use std::path::PathBuf;
use std::thread;

use crossbeam::thread::scope;
use fs2;
use humantime::{format_duration, FormattedDuration};
use indicatif::ProgressBar;
use indicatif::{MultiProgress, ProgressStyle};
use std::time::{Duration, Instant};
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
            let start = Instant::now();
            let prog_bar = ProgressBar::new(fs2::free_space(&p).unwrap());

            prog_bar.set_style(ProgressStyle::default_bar().template(
                "[{elapsed_precise}] {bar:40.cyan/blue} {bytes:>7}/{total_bytes:7} :: {eta_precise} {msg}",
            ));
            multi.add(prog_bar.clone());

            prog_bar.set_message(&format!("{}", p.display()));
            s.spawn(move |_| {
                for _ in 0..10 {
                    prog_bar.inc(1 << 25);
                    thread::sleep(Duration::from_secs(1));
                }
                //                bar.finish_at_current_pos();
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
