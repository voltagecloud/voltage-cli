#[tokio::main]
async fn main() -> std::process::ExitCode {
    voltage_cli::run().await
}
