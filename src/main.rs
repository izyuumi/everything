use clap::Parser;

#[derive(Parser)]
#[command(version, about)]
struct Arguments {}

fn main() {
    Arguments::parse();
}
