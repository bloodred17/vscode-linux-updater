mod commands;

use clap::{Parser};

// #[derive(Parser, Debug)]
// #[command(version, about, long_about = None)]
// struct ArgsDemo {
//     #[arg(short, long)]
//     name: String,

//     #[arg(short, long, default_value_t = 1)]
//     count: u8,
// }

// fn main() {
//     let args_demo = ArgsDemo::parse();

//     for _ in 0..args_demo.count {
//         print!("Hello {}!", args_demo.name);
//     }
// }

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: commands::Command,
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        commands::Command::Install { version } => {
            print!("Version {version}!");
        },
        _ => {
            print!("Others")
        }
    }
}