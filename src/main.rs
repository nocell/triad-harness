use clap::Parser;
use triad::cli::Cli;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match triad::execute(cli).await {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(3);
        }
    }
}
