mod api;
mod runtime;
mod store;

use std::{error::Error, path::PathBuf};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    if std::env::args()
        .skip(1)
        .any(|arg| arg == "--help" || arg == "-h")
    {
        println!(
            "ene-host\n  XDG_RUNTIME_DIR/ene-host/connection.json に起動ごとの接続情報を公開します。\n  ENE_HOST_STATE_DIR でDB保存先を指定できます。\n  現在は認証・初期化・履歴・通知購読のみ対応しています。"
        );
        return Ok(());
    }
    if std::env::args().len() != 1 {
        return Err("unsupported argument (use --help)".into());
    }
    let root =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is required")?);
    let runtime = runtime::Runtime::acquire(&root)?;
    let state = if let Some(path) = std::env::var_os("ENE_HOST_STATE_DIR") {
        PathBuf::from(path)
    } else {
        let base = if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
            PathBuf::from(path)
        } else {
            PathBuf::from(std::env::var_os("HOME").ok_or("HOME is required")?).join(".local/state")
        };
        std::fs::create_dir_all(&base)?;
        base.join("ene")
    };
    runtime::private_directory(&state)?;
    let database = state.join("host.sqlite3");
    let file = runtime::private_file(&database)?;
    drop(file);
    let store = store::Store::open(database)
        .await
        .map_err(|_| "database initialization failed")?;
    let app = api::App::new(store, runtime.token.clone());
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    runtime.publish(format!("http://{}/v1", listener.local_addr()?))?;
    eprintln!("ene-host: loopback service ready (history and notifications; chat not yet enabled)");
    let shutdown = app.shutdown.clone();
    axum::serve(listener, api::router(app))
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            {
                if let Ok(mut terminate) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                {
                    tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
                    shutdown.send_replace(true);
                    return;
                }
            }
            let _ = tokio::signal::ctrl_c().await;
            shutdown.send_replace(true);
        })
        .await?;
    drop(runtime);
    Ok(())
}
