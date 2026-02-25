mod config;  
mod snapshot;
mod sim;
mod cli;

use std::{fs, path::PathBuf};
use chrono::Local;
use clap::Parser;

use cli::Cli;
use sim::Simulator;

fn main() -> std::io::Result<()> {
    let args = Cli::parse();

    let ts = Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
    let mode = if args.recluster { "recluster" } else { "no_recluster" };
    let run_dir = PathBuf::from(format!("{}/{}/{}", args.out, mode, ts));
    let snap_dir = run_dir.join("snapshots");

    fs::create_dir_all(&snap_dir)?;

    let num_tables: u16 = 6;
    let pages_per_table: u32 = 200;

    let mut sim = Simulator::new(
        args.seed,
        args.frames,
        num_tables,
        pages_per_table,
    );

    for t in 1..=args.ticks {
        let recluster = args.recluster && t % args.recluster_every == 0;

        let snap = sim.tick(t, recluster);

        if t % args.snapshot_every == 0 {
            let fname = format!("{:06}.json", t);
            let path = snap_dir.join(fname);

            let mut s = serde_json::to_string(&snap).unwrap();
            s.push('\n');
            fs::write(path, s)?;
        }
    }

    println!("Run written to {}", run_dir.display());
    Ok(())
}
