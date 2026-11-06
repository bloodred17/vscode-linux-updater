use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum Command {
    Install {
        #[arg(short, long, default_value = "latest")]
        version: String,
        /// Create a COSMIC/application-menu launcher.
        #[arg(long)]
        desktop: bool,
    },

    Check,
    Update {
        /// Activate the already staged release without contacting upstream.
        #[arg(long)]
        activate: bool,
    },
    Status,
    Rollback,
    Doctor,
    Clean {
        /// Delete the candidates shown by a plain `clean` invocation.
        #[arg(long)]
        yes: bool,
    },
}
