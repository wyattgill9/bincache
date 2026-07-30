use clap::Parser;

#[derive(Parser)]
struct Args {
    name: String
}

fn main() {
    let args = Args::parse();
    println!("Hello, {0}", args.name);
}
