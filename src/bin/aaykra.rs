#[tokio::main]
async fn main() {
    std::process::exit(aaykra::cli::run_aaykra_alias().await);
}
