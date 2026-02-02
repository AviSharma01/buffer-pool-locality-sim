mod config;
mod snapshot;
mod sim;

use std::{fs, path::PathBuf};
use chrono::Local;

use config::Config;
use sim::Simulator;

fn main() -> std::io::Result<()> {
    let cfg = Config::default();

    let ts = Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
    let run_dir = PathBuf::from(format!("../outputs/runs/{ts}"));
    let snap_dir = run_dir.join("snapshots");

    fs::create_dir_all(&snap_dir)?;

    let mut sim = Simulator::new(cfg.seed, cfg.frames);

    for t in 1..=cfg.ticks {
        let (mut snap, _cost) = sim.tick(t);

        // later you’ll set this when reclustering happens:
        if t % cfg.recluster_every == 0 { snap.recluster = true; }

        if t % cfg.snapshot_every == 0 {
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
