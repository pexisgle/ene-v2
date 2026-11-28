use clap::Parser as _;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    ene_host::run(ene_host::HostArgs::parse()).await
}
