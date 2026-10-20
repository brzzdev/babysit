use clap::Parser;

#[derive(Parser)]
#[command(name = "babysit", version, about, long_about = None)]
struct Cli {}

fn main() {
    Cli::parse();
}
