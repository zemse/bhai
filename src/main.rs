//! The command-line frontend over the shared bhai library.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    bhai::cli::entry().await
}
