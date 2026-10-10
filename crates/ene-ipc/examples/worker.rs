//! A Rust child worker with an RPC result and bounded progress channel.
//! This example only computes values; it performs no filesystem mutations.
use ene_ipc::remoc::{
    self, Cfg, Connect, ConnectExt,
    rch::mpsc,
    rtc::{self, CallError, ServerShared},
};
use std::{process::Stdio, sync::Arc, time::Duration};

#[rtc::remote]
trait Worker {
    async fn count(&self, count: u32, progress: mpsc::Sender<u32>) -> Result<u32, CallError>;
}

struct WorkerEngine;
impl Worker for WorkerEngine {
    async fn count(&self, count: u32, progress: mpsc::Sender<u32>) -> Result<u32, CallError> {
        for index in 0..count {
            if progress.send(index).await.is_err() {
                return Ok(index);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        Ok(count)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|arg| arg == "--child") {
        let (server, client) =
            WorkerServerShared::<_, remoc::codec::Default>::new(Arc::new(WorkerEngine));
        Connect::io::<_, _, WorkerClient, WorkerClient, remoc::codec::Default>(
            Cfg::default(),
            tokio::io::stdin(),
            tokio::io::stdout(),
        )
        .provide(client)
        .await?;
        server.serve().await?;
        return Ok(());
    }
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .arg("--child")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let client: WorkerClient = Connect::io::<_, _, _, _, remoc::codec::Default>(
        Cfg::default(),
        child.stdout.take().unwrap(),
        child.stdin.take().unwrap(),
    )
    .consume()
    .await?;
    let (progress, mut updates) = mpsc::channel();
    let (result, ()) = tokio::join!(client.count(3, progress), async {
        while let Some(index) = updates.recv().await.unwrap() {
            println!("progress: {index}");
        }
    });
    println!("result: {}", result?);
    drop(client);
    if !tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await??
        .success()
    {
        return Err("worker failed".into());
    }
    Ok(())
}
