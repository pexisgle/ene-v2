//! Runs the authenticated experimental host on its loopback address.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stopped = tokio_util::sync::CancellationToken::new();
    let signal = stopped.clone();
    ctrlc::set_handler(move || signal.cancel())?;
    ene_host_test::server::serve("127.0.0.1:8080".parse()?, stopped.cancelled_owned()).await
}
