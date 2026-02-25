use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about)]
pub struct Cli {
    #[arg(long, default_value_t = 5000)]
    pub ticks: u64,

    #[arg(long, default_value_t = 64)]
    pub frames: usize,

    #[arg(long, default_value_t = 42)]
    pub seed: u64,

    #[arg(long, default_value_t = 10)]
    pub snapshot_every: u64,

    #[arg(long, default_value_t = 500)]
    pub recluster_every: u64,

    #[arg(long, default_value = "../outputs/runs")]
    pub out: String,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub recluster: bool,
}
