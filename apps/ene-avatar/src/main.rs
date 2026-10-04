mod application;
mod ipc;
mod render;
mod wayland;

use ene_protocol::avatar::AvatarEvent;
use ipc::emit;
fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    use clap::Parser;
    let args = application::Args::parse();
    let mut ipc = ipc::Ipc::new(args.standalone);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        application::run(args, &mut ipc)
    }));
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("ene-avatar: {error}");
            emit(AvatarEvent::Error {
                message: error.to_string(),
            });
            drop(ipc);
            std::process::exit(1);
        }
        Err(_) => {
            emit(AvatarEvent::Error{message:"キャラクターの描画処理に失敗しました。GPU・モデルを確認して再起動してください。".into()});
            drop(ipc);
            std::process::exit(1);
        }
    }
}
