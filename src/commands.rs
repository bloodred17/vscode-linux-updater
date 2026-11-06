use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum Command {
    Install {
        #[arg(short, long, default_value = "latest")]
        version: String,
    },

    Check,
    Update,
    Status,
    Rollback,
    Doctor,
    Clean,
}
