//! Development console for the real renderer's Remoc RPC and channels.
use clap::{Parser, Subcommand, ValueEnum};
use ene_ipc::renderer::{
    AvatarConfig, AvatarId, AvatarInstance, AvatarPlacement, AvatarState, MotionState, Renderer,
    RendererClient, RendererConfig, RendererError, RendererMessage, RendererSession, RendererState,
};
use remoc::{Cfg, Connect, ConnectExt};
use std::{
    collections::HashMap,
    io::{self, BufRead, IsTerminal, Write},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    sync::mpsc,
    time::timeout,
};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Client = RendererSession;
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(about = "Control ene-avatar interactively over IPC")]
struct Args {
    /// Renderer executable (defaults to ene-avatar beside this build's examples directory).
    #[arg(long)]
    renderer: Option<PathBuf>,
}

#[derive(Parser)]
#[command(no_binary_name = true, disable_help_subcommand = true)]
struct Input {
    #[command(subcommand)]
    command: ConsoleCommand,
}

#[derive(Subcommand)]
enum ConsoleCommand {
    /// Add an avatar (defaults to the cached Seed-san sample).
    Add {
        model: Option<String>,
        #[arg(long)]
        monitor: Option<String>,
    },
    Remove {
        id: u64,
    },
    Model {
        id: u64,
        path: String,
    },
    Motion {
        id: u64,
        path: String,
    },
    MotionClear {
        id: u64,
    },
    Play {
        id: u64,
    },
    Pause {
        id: u64,
    },
    Loop {
        id: u64,
        value: Switch,
    },
    Size {
        id: u64,
        percent: u32,
    },
    /// Set the shared FPS cap; zero disables the cap.
    Fps {
        value: u32,
    },
    Recenter {
        id: u64,
    },
    /// List renderer-owned avatar IDs and compare them with the console's last-sent IDs.
    List,
    /// Show the latest renderer state and the separate last-sent settings.
    State,
    Help,
    Quit,
}

#[derive(Clone, Copy, ValueEnum)]
enum Switch {
    On,
    Off,
}

fn parse(line: &str) -> Result<Option<ConsoleCommand>, String> {
    let words = shell_words::split(line).map_err(|error| error.to_string())?;
    if words.is_empty() {
        return Ok(None);
    }
    Input::try_parse_from(words)
        .map(|input| Some(input.command))
        .map_err(|error| error.to_string())
}

#[derive(Clone, Copy)]
enum AssetKind {
    Model,
    Motion,
}

struct AssetPaths {
    home: Option<PathBuf>,
    cache: Option<PathBuf>,
}

impl Default for AssetPaths {
    fn default() -> Self {
        let home =
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
        let cache = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(|path| PathBuf::from(path).join("ene/cache"))
        } else {
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|path| path.join(".cache")))
                .map(|path| path.join("ene"))
        };
        Self { home, cache }
    }
}

impl AssetPaths {
    fn resolve(&self, value: &str, kind: AssetKind) -> Result<String, String> {
        let sample = match (kind, value) {
            (AssetKind::Model, "seed") => Some("models/Seed-san.vrm"),
            (AssetKind::Motion, "sample") => Some("motions/three-vrm-sample.vrma"),
            (AssetKind::Motion, "dance") => Some("motions/quaternius-dance.vrma"),
            (AssetKind::Motion, "walk") => Some("motions/josie-walk.vrma"),
            (AssetKind::Motion, "slow-run") => Some("motions/josie-slow-run.vrma"),
            _ => None,
        };
        let path = if let Some(sample) = sample {
            self.cache
                .as_ref()
                .ok_or("Cannot locate sample cache; provide an explicit path")?
                .join(sample)
        } else if value == "~" {
            self.home
                .clone()
                .ok_or("Cannot expand ~: home directory is unavailable")?
        } else if let Some(relative) = value
            .strip_prefix("~/")
            .or_else(|| value.strip_prefix("~\\"))
        {
            self.home
                .as_ref()
                .ok_or("Cannot expand ~: home directory is unavailable")?
                .join(relative)
        } else {
            return Ok(value.to_owned());
        };
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| "Path is not valid UTF-8".into())
    }
}

// Prepared console edits retain settings until the renderer acknowledges the call.
#[derive(Debug, Clone, PartialEq)]
enum Edit {
    AddAvatar {
        avatar: AvatarInstance,
    },
    SetRenderConfig {
        config: RendererConfig,
    },
    RemoveAvatar {
        avatar_id: AvatarId,
    },
    Recenter {
        avatar_id: AvatarId,
    },
    SetAvatarConfig {
        avatar_id: AvatarId,
        config: AvatarConfig,
    },
}

struct Sent {
    next_id: Option<u64>,
    avatars: HashMap<AvatarId, AvatarConfig>,
    render: RendererConfig,
    assets: AssetPaths,
}

impl Default for Sent {
    fn default() -> Self {
        Self {
            next_id: Some(1),
            avatars: HashMap::new(),
            render: RendererConfig::default(),
            assets: AssetPaths::default(),
        }
    }
}

impl Sent {
    // Commit the settings cache only after a successful RPC result.
    fn request(&self, command: ConsoleCommand) -> Result<Edit, String> {
        let request = match command {
            ConsoleCommand::Add { model, monitor } => Edit::AddAvatar {
                avatar: AvatarInstance {
                    id: AvatarId(self.next_id.ok_or("Avatar IDs exhausted")?),
                    config: AvatarConfig {
                        model: self
                            .assets
                            .resolve(model.as_deref().unwrap_or("seed"), AssetKind::Model)?,
                        ..Default::default()
                    },
                    placement: AvatarPlacement {
                        monitor,
                        ..Default::default()
                    },
                },
            },
            ConsoleCommand::Fps { value } => Edit::SetRenderConfig {
                config: RendererConfig { fps: value },
            },
            ConsoleCommand::Remove { id } => {
                self.config(id)?;
                Edit::RemoveAvatar {
                    avatar_id: AvatarId(id),
                }
            }
            ConsoleCommand::Recenter { id } => {
                self.config(id)?;
                Edit::Recenter {
                    avatar_id: AvatarId(id),
                }
            }
            command => {
                let id = match &command {
                    ConsoleCommand::Model { id, .. }
                    | ConsoleCommand::Motion { id, .. }
                    | ConsoleCommand::MotionClear { id }
                    | ConsoleCommand::Play { id }
                    | ConsoleCommand::Pause { id }
                    | ConsoleCommand::Loop { id, .. }
                    | ConsoleCommand::Size { id, .. } => *id,
                    _ => return Err("This command does not send a renderer request".into()),
                };
                let mut config = self.config(id)?.clone();
                match command {
                    ConsoleCommand::Model { path, .. } => {
                        config.model = self.assets.resolve(&path, AssetKind::Model)?
                    }
                    ConsoleCommand::Motion { path, .. } => {
                        config.motion = Some(self.assets.resolve(&path, AssetKind::Motion)?)
                    }
                    ConsoleCommand::MotionClear { .. } => config.motion = None,
                    ConsoleCommand::Play { .. } => config.motion_playing = true,
                    ConsoleCommand::Pause { .. } => config.motion_playing = false,
                    ConsoleCommand::Loop { value, .. } => {
                        config.motion_loop = matches!(value, Switch::On)
                    }
                    ConsoleCommand::Size { percent, .. } => config.size_percent = percent,
                    _ => return Err("Unsupported setting command".into()),
                }
                Edit::SetAvatarConfig {
                    avatar_id: AvatarId(id),
                    config,
                }
            }
        };
        match &request {
            Edit::AddAvatar { avatar } => avatar.config.validate()?,
            Edit::SetAvatarConfig { config, .. } => config.validate()?,
            _ => {}
        }
        Ok(request)
    }

    fn config(&self, id: u64) -> Result<&AvatarConfig, String> {
        self.avatars
            .get(&AvatarId(id))
            .ok_or_else(|| format!("Unknown or removed avatar ID: {id}"))
    }

    fn commit(&mut self, request: &Edit) {
        match request {
            Edit::AddAvatar { avatar } => {
                self.avatars.insert(avatar.id, avatar.config.clone());
                self.next_id = avatar.id.0.checked_add(1);
                println!("[sent avatar {}] added", avatar.id);
            }
            Edit::SetAvatarConfig { avatar_id, config } => {
                self.avatars.insert(*avatar_id, config.clone());
            }
            Edit::RemoveAvatar { avatar_id } => {
                self.avatars.remove(avatar_id);
            }
            Edit::SetRenderConfig { config } => self.render = *config,
            _ => {}
        }
    }
}

fn default_renderer() -> Result<PathBuf, Error> {
    let executable = std::env::current_exe()?;
    let build_dir = executable
        .parent()
        .and_then(|examples| examples.parent())
        .ok_or("Cannot find the build directory; use --renderer PATH")?;
    Ok(build_dir.join(format!("ene-avatar{}", std::env::consts::EXE_SUFFIX)))
}

fn spawn(renderer: PathBuf) -> Result<Child, Error> {
    let mut command = Command::new(renderer);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    // Ctrl+C belongs to the console, which requests an orderly renderer shutdown.
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x0000_0200); // CREATE_NEW_PROCESS_GROUP
    Ok(command.spawn()?)
}

async fn connect(child: &mut Child) -> Result<Client, Error> {
    let input = child.stdout.take().ok_or("Renderer stdout is not piped")?;
    let output = child.stdin.take().ok_or("Renderer stdin is not piped")?;
    Ok(timeout(
        TIMEOUT,
        Connect::io::<_, _, Client, Client, remoc::codec::Default>(Cfg::default(), input, output)
            .consume(),
    )
    .await
    .map_err(|_| "Renderer connection timed out after 5 seconds")??)
}

async fn execute(renderer: &mut RendererClient, command: Edit) -> Result<(), RendererError> {
    match command {
        Edit::AddAvatar { avatar } => renderer.add_avatar(avatar).await,
        Edit::SetRenderConfig { config } => renderer.set_render_config(config).await,
        Edit::RemoveAvatar { avatar_id } => renderer.remove_avatar(avatar_id).await,
        Edit::Recenter { avatar_id } => renderer.recenter(avatar_id).await,
        Edit::SetAvatarConfig { avatar_id, config } => {
            renderer.set_avatar_config(avatar_id, config).await
        }
    }
}

fn input_lines() -> Result<mpsc::Receiver<io::Result<String>>, Error> {
    let (sender, receiver) = mpsc::channel(16);
    // Tokio stdin uses an uncancellable blocking read that can hold runtime shutdown
    // open. This detached thread is not joined; an idle read can outlive the
    // session without delaying process exit.
    std::thread::Builder::new()
        .name("console-input".into())
        .spawn(move || {
            for line in io::stdin().lock().lines() {
                let failed = line.is_err();
                if sender.blocking_send(line).is_err() || failed {
                    break;
                }
            }
        })?;
    Ok(receiver)
}

fn prompt(interactive: bool) -> io::Result<()> {
    if interactive {
        print!("avatar> ");
        io::stdout().flush()?;
    }
    Ok(())
}

fn help() {
    println!(
        r#"
アバター
  list                           ID・状態・位置の一覧と登録の食い違いを表示
  add [MODEL] [--monitor NAME]   追加（MODEL省略時はSeed-san）
  remove ID                      削除
  model ID PATH                  モデルを変更

モーション
  motion ID PATH                 モーションを変更
  motion-clear ID                モーションを解除
  play ID                        再生・再開
  pause ID                       一時停止
  loop ID on|off                 繰り返しを切り替え

表示
  size ID PERCENT                サイズを変更（50〜200%）
  fps VALUE                      全体のFPS上限を変更（0は上限なし）
  recenter ID                    現在のモニター中央へ移動

コンソール
  state                          実際の状態と送信済み設定を詳しく表示
  help                           このヘルプを表示
  quit                           描画プロセスを終了して閉じる（Ctrl+Cも可）

まず試す
  add                            サンプルモデルを追加 → IDが表示される
  list                           操作対象のIDと状態を確認
  motion 1 walk                  ID 1のアバターで歩行を再生
  pause 1                        ID 1のアバターを一時停止

サンプル名: モデル seed / モーション sample, dance, walk, slow-run
  未取得なら別の端末で just fetch-assets を実行してください。
パス: ~/ に対応。空白を含むパスは "~/Models/Seed san.vrm" のように引用符で囲みます。
ID: 追加時に表示される番号です。削除後も再利用しません。
出力: [sent] は送信済み設定です。反映結果は [renderer] と state で確認できます。
"#
    );
}

fn avatar_summary(id: AvatarId, avatar: &AvatarState) -> String {
    let status = if avatar.error.is_some() {
        "error"
    } else if avatar.loading {
        "loading"
    } else {
        "ready"
    };
    let placement = &avatar.placement;
    let error = avatar
        .error
        .as_ref()
        .map(|message| format!(" | error={message:?}"))
        .unwrap_or_default();
    format!(
        "[renderer avatar {id}] {status} | motion={:?} | monitor={} | position=({},{}){error}",
        avatar.motion,
        placement.monitor.as_deref().unwrap_or("default"),
        placement.x,
        placement.y
    )
}

fn avatar_list(state: &RendererState, sent: &Sent) -> String {
    let mut ids: Vec<_> = state
        .avatars
        .keys()
        .chain(sent.avatars.keys())
        .copied()
        .collect();
    ids.sort_by_key(|id| id.0);
    ids.dedup();
    let mut output = format!(
        "[list] renderer={} / sent={}\n",
        state.avatars.len(),
        sent.avatars.len()
    );
    if ids.is_empty() {
        output.push_str("アバターはありません。add で追加できます。\n");
        return output;
    }
    output.push_str(&format!(
        "{:<5} {:<10} {:<12} {:<18} {:<15} {}\n",
        "ID", "STATUS", "MOTION", "MONITOR", "POSITION", "REGISTRY"
    ));
    for id in ids {
        let avatar = state.avatars.get(&id);
        let registered = sent.avatars.contains_key(&id);
        let registry = match (avatar.is_some(), registered) {
            (true, true) => "both",
            (true, false) => "renderer-only",
            (false, _) => "sent-only",
        };
        let (status, motion, monitor, position) = match avatar {
            Some(avatar) => (
                if avatar.error.is_some() {
                    "error"
                } else if avatar.loading {
                    "loading"
                } else {
                    "ready"
                },
                match &avatar.motion {
                    MotionState::None => "none",
                    MotionState::Loading => "loading",
                    MotionState::Playing => "playing",
                    MotionState::Paused => "paused",
                    MotionState::Finished => "finished",
                    MotionState::Failed { .. } => "failed",
                },
                avatar
                    .placement
                    .monitor
                    .as_deref()
                    .unwrap_or("default")
                    .escape_default()
                    .to_string(),
                format!("({},{})", avatar.placement.x, avatar.placement.y),
            ),
            None => ("absent", "-", "-".into(), "-".into()),
        };
        output.push_str(&format!(
            "{:<5} {:<10} {:<12} {:<18} {:<15} {}\n",
            id.0, status, motion, monitor, position, registry
        ));
    }
    output.push_str(
        "REGISTRY: both=両方に登録 / renderer-only=描画側のみ / sent-only=送信済みのみ\n",
    );
    output.push_str("追加・削除の反映中は一時的に異なることがあります。設定値の一致は判定しません。詳細は state。\n");
    output
}

fn print_state(state: &RendererState, previous: Option<&RendererState>) {
    if previous.is_none_or(|old| old.config != state.config) {
        println!("[renderer] config: {:?}", state.config);
    }
    let mut ids: Vec<_> = state.avatars.keys().collect();
    ids.sort_by_key(|id| id.0);
    for id in ids {
        let avatar = &state.avatars[id];
        if previous.is_none_or(|old| old.avatars.get(id) != Some(avatar)) {
            println!("{}", avatar_summary(*id, avatar));
            if previous.is_none_or(|old| {
                old.avatars
                    .get(id)
                    .is_none_or(|old_avatar| old_avatar.model != avatar.model)
            }) && let Some(credit) = &avatar.model
            {
                println!("[model avatar {id}] {credit}");
            }
        }
    }
    if let Some(old) = previous {
        for id in old
            .avatars
            .keys()
            .filter(|id| !state.avatars.contains_key(id))
        {
            println!("[renderer avatar {id}] absent from latest state");
        }
    }
}

fn print_message(message: &RendererMessage) {
    match message {
        RendererMessage::Event { avatar_id, event } => {
            println!("[event avatar {avatar_id}] {event:?}")
        }
        RendererMessage::Error {
            avatar_id: Some(id),
            message,
        } => eprintln!("[error avatar {id}] {message}"),
        RendererMessage::Error {
            avatar_id: None,
            message,
        } => eprintln!("[renderer error] {message}"),
    }
}

async fn session(child: &mut Child, client: &mut Client) -> Result<(), Error> {
    // Register the handler before reading input; retain one future across select calls.
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    tokio::select! {
        result = timeout(TIMEOUT, client.renderer.initialize(RendererConfig::default(), Vec::new())) => result.map_err(|_| "Renderer initialization timed out")??,
        result = &mut interrupt => { result?; return Ok(()); }
    }
    let mut lines = input_lines()?;
    let interactive = io::stdin().is_terminal();
    let mut sent = Sent::default();
    let mut previous = client.states.borrow_and_update()?.clone();
    help();
    print_state(&previous, None);
    prompt(interactive)?;
    loop {
        tokio::select! {
            status = child.wait() => return Err(format!("Renderer exited: {}", status?).into()),
            result = &mut interrupt => {
                result?;
                println!("\n[console] Ctrl+C; requesting shutdown");
                return Ok(());
            }
            result = client.states.changed() => {
                result.map_err(|error| format!("Renderer state channel disconnected: {error}"))?;
                let state = client.states.borrow_and_update()?.clone();
                print_state(&state, Some(&previous));
                previous = state;
                prompt(interactive)?;
            }
            result = client.messages.recv() => {
                let message = result?.ok_or("Renderer event channel disconnected")?;
                print_message(&message);
                prompt(interactive)?;
            }
            line = lines.recv() => {
                let Some(line) = line else {
                    println!("[console] EOF; requesting shutdown");
                    return Ok(());
                };
                match parse(&line?) {
                    Ok(Some(ConsoleCommand::Quit)) => return Ok(()),
                    Ok(Some(ConsoleCommand::Help)) => help(),
                    Ok(Some(ConsoleCommand::List)) => {
                        print!("{}", avatar_list(&*client.states.borrow()?, &sent));
                    }
                    Ok(Some(ConsoleCommand::State)) => {
                        let state = client.states.borrow()?;
                        println!("[renderer] config: {:?}", state.config);
                        let mut ids: Vec<_> = state.avatars.keys().collect();
                        ids.sort_by_key(|id| id.0);
                        for id in ids { println!("[renderer avatar {id}] {:#?}", state.avatars[id]); }
                        println!("[sent renderer config] {:?}", sent.render);
                        let mut ids: Vec<_> = sent.avatars.keys().collect();
                        ids.sort_by_key(|id| id.0);
                        for id in ids { println!("[sent avatar {id}] {:#?}", sent.avatars[id]); }
                    }
                    Ok(Some(command)) => match sent.request(command) {
                        Ok(request) => {
                            println!("[sent request] {request:?}");
                            // Keep receiving events and state while the RPC result is pending.
                            let operation = execute(&mut client.renderer, request.clone());
                            tokio::pin!(operation);
                            let deadline = tokio::time::sleep(TIMEOUT);
                            tokio::pin!(deadline);
                            loop {
                                tokio::select! {
                                    result = &mut operation => {
                                        match result {
                                            Ok(()) => {
                                                sent.commit(&request);
                                                println!("[accepted request] {request:?}");
                                            }
                                            Err(RendererError::Rejected(message)) => eprintln!("[command error] {message}"),
                                            Err(error) => return Err(error.into()),
                                        }
                                        break;
                                    }
                                    _ = &mut deadline => return Err("Renderer operation timed out".into()),
                                    result = &mut interrupt => { result?; return Ok(()); }
                                    status = child.wait() => return Err(format!("Renderer exited: {}", status?).into()),
                                    message = client.messages.recv() => print_message(&message?.ok_or("Renderer event channel disconnected")?),
                                    result = client.states.changed() => {
                                        result?;
                                        let state = client.states.borrow_and_update()?.clone();
                                        print_state(&state, Some(&previous));
                                        previous = state;
                                    }
                                }
                            }
                        }
                        Err(error) => eprintln!("[command error] {error}"),
                    },
                    Ok(None) => {}
                    Err(error) => eprintln!("[command error] {error}"),
                }
                prompt(interactive)?;
            }
        }
    }
}

async fn shutdown(child: &mut Child, mut client: Client) -> Result<(), Error> {
    if let Some(status) = child.try_wait()? {
        println!("[renderer] exited: {status}");
        return Ok(());
    }
    let result = {
        let stopping = timeout(TIMEOUT, async {
            client.renderer.shutdown().await?;
            Ok::<_, Error>(child.wait().await?)
        });
        tokio::pin!(stopping);
        let mut messages_open = true;
        let mut states_open = true;
        loop {
            tokio::select! {
                result = &mut stopping => break result,
                message = client.messages.recv(), if messages_open => match message {
                    Ok(Some(message)) => print_message(&message),
                    _ => messages_open = false,
                },
                result = client.states.changed(), if states_open => match result {
                    Ok(()) => match client.states.borrow_and_update() {
                        Ok(state) => print_state(&state, None),
                        Err(_) => states_open = false,
                    },
                    Err(_) => states_open = false,
                }
            }
        }
    };
    match result {
        Ok(Ok(status)) => {
            // The child may exit before select visits its final buffered events.
            while let Ok(message) = client.messages.try_recv() {
                print_message(&message);
            }
            println!("[renderer] exited: {status}");
            if status.success() {
                Ok(())
            } else {
                Err(format!("Renderer shutdown failed: {status}").into())
            }
        }
        result => {
            eprintln!(
                "[console] Shutdown failed or exceeded 5 seconds; stopping renderer: {result:?}"
            );
            child.kill().await?;
            Err("Renderer required forced termination".into())
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args = Args::parse();
    let renderer = match args.renderer {
        Some(path) => path,
        None => default_renderer()?,
    };
    println!("[console] starting {}", renderer.display());
    let mut child = spawn(renderer)?;
    let connected = tokio::select! {
        result = connect(&mut child) => result,
        result = tokio::signal::ctrl_c() => {
            result?;
            println!("\n[console] Ctrl+C during connection; stopping renderer");
            child.kill().await?;
            return Ok(());
        }
    };
    let mut client = match connected {
        Ok(client) => client,
        Err(error) => {
            eprintln!("[console] Renderer connection failed: {error}");
            child.kill().await?;
            return Err(error);
        }
    };
    let result = session(&mut child, &mut client).await;
    let stopped = shutdown(&mut child, client).await;
    result.and(stopped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(sent: &Sent, line: &str) -> Edit {
        sent.request(parse(line).unwrap().unwrap()).unwrap()
    }

    #[test]
    fn list_uses_renderer_state_and_reports_missing_ids_in_both_directions() {
        assert!(matches!(parse("list").unwrap(), Some(ConsoleCommand::List)));
        let state = RendererState {
            avatars: HashMap::from([
                (
                    AvatarId(20),
                    AvatarState {
                        loading: false,
                        motion: MotionState::Playing,
                        ..Default::default()
                    },
                ),
                (
                    AvatarId(2),
                    AvatarState {
                        loading: false,
                        error: Some("bad model".into()),
                        motion: MotionState::Failed {
                            message: "bad motion".into(),
                        },
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };
        let sent = Sent {
            avatars: HashMap::from([
                (AvatarId(2), AvatarConfig::default()),
                (AvatarId(99), AvatarConfig::default()),
            ]),
            ..Default::default()
        };
        let output = avatar_list(&state, &sent);
        let rows: Vec<_> = output
            .lines()
            .filter(|line| line.starts_with(|character: char| character.is_ascii_digit()))
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .collect();
        assert_eq!(
            rows,
            vec![
                vec!["2", "error", "failed", "default", "(24,24)", "both"],
                vec![
                    "20",
                    "ready",
                    "playing",
                    "default",
                    "(24,24)",
                    "renderer-only"
                ],
                vec!["99", "absent", "-", "-", "-", "sent-only"],
            ]
        );
        assert!(output.contains("設定値の一致は判定しません"));
    }

    #[test]
    fn empty_list_explains_how_to_add_an_avatar() {
        let output = avatar_list(&RendererState::default(), &Sent::default());
        assert!(output.contains("renderer=0 / sent=0"));
        assert!(output.contains("add で追加できます"));
    }

    #[test]
    fn sample_shortcuts_and_home_paths_resolve_without_shell_expansion() {
        let assets = AssetPaths {
            home: Some(PathBuf::from("home")),
            cache: Some(PathBuf::from("cache")),
        };
        for (kind, name, expected) in [
            (AssetKind::Model, "seed", "models/Seed-san.vrm"),
            (AssetKind::Motion, "sample", "motions/three-vrm-sample.vrma"),
            (AssetKind::Motion, "dance", "motions/quaternius-dance.vrma"),
            (AssetKind::Motion, "walk", "motions/josie-walk.vrma"),
            (AssetKind::Motion, "slow-run", "motions/josie-slow-run.vrma"),
        ] {
            assert_eq!(
                PathBuf::from(assets.resolve(name, kind).unwrap()),
                PathBuf::from("cache").join(expected)
            );
        }
        assert_eq!(
            PathBuf::from(
                assets
                    .resolve("~/Models/Seed san.vrm", AssetKind::Model)
                    .unwrap()
            ),
            PathBuf::from("home").join("Models/Seed san.vrm")
        );
        for path in [
            "./walk",
            "~someone/model.vrm",
            "$HOME/model.vrm",
            "custom motion.vrma",
        ] {
            assert_eq!(assets.resolve(path, AssetKind::Motion).unwrap(), path);
        }
        let sent = Sent {
            assets,
            ..Default::default()
        };
        let Edit::AddAvatar { avatar } = request(&sent, "add --monitor DP-1") else {
            panic!()
        };
        assert_eq!(
            PathBuf::from(avatar.config.model),
            PathBuf::from("cache/models/Seed-san.vrm")
        );
        assert_eq!(avatar.placement.monitor.as_deref(), Some("DP-1"));
        let unavailable = AssetPaths {
            home: None,
            cache: None,
        };
        assert!(unavailable.resolve("seed", AssetKind::Model).is_err());
        assert!(
            unavailable
                .resolve("~/model.vrm", AssetKind::Model)
                .is_err()
        );
        assert_eq!(
            unavailable.resolve("model.vrm", AssetKind::Model).unwrap(),
            "model.vrm"
        );
    }

    #[test]
    fn automatic_state_summary_is_one_line_and_includes_errors() {
        let mut avatar = AvatarState {
            loading: false,
            ..Default::default()
        };
        assert_eq!(
            avatar_summary(AvatarId(1), &avatar),
            "[renderer avatar 1] ready | motion=None | monitor=default | position=(24,24)"
        );
        avatar.error = Some("cannot load\nmodel".into());
        let summary = avatar_summary(AvatarId(1), &avatar);
        assert!(summary.contains("] error |"));
        assert!(summary.contains(r"cannot load\nmodel"));
        assert!(!summary.contains('\n'));
    }

    #[test]
    fn quoted_paths_and_monitor_names_are_preserved() {
        let sent = Sent::default();
        let Edit::AddAvatar { avatar } =
            request(&sent, r#"add "C:\Models\Seed san.vrm" --monitor 'DP 1'"#)
        else {
            panic!()
        };
        assert_eq!(avatar.config.model, r"C:\Models\Seed san.vrm");
        assert_eq!(avatar.placement.monitor.as_deref(), Some("DP 1"));
        assert!(parse("add 'unterminated").is_err());
    }

    #[test]
    fn setting_edits_preserve_other_settings_and_other_avatars() {
        let original = AvatarConfig {
            model: "model.vrm".into(),
            motion: Some("motion.vrma".into()),
            motion_playing: false,
            motion_loop: false,
            size_percent: 150,
        };
        let sent = Sent {
            avatars: HashMap::from([
                (AvatarId(1), original.clone()),
                (AvatarId(2), original.clone()),
            ]),
            ..Default::default()
        };
        for (line, expected) in [
            (
                "model 1 other.vrm",
                AvatarConfig {
                    model: "other.vrm".into(),
                    ..original.clone()
                },
            ),
            (
                "motion 1 'other motion.vrma'",
                AvatarConfig {
                    motion: Some("other motion.vrma".into()),
                    ..original.clone()
                },
            ),
            (
                "motion-clear 1",
                AvatarConfig {
                    motion: None,
                    ..original.clone()
                },
            ),
            (
                "play 1",
                AvatarConfig {
                    motion_playing: true,
                    ..original.clone()
                },
            ),
            ("pause 1", original.clone()),
            (
                "loop 1 on",
                AvatarConfig {
                    motion_loop: true,
                    ..original.clone()
                },
            ),
            ("loop 1 off", original.clone()),
            (
                "size 1 50",
                AvatarConfig {
                    size_percent: 50,
                    ..original.clone()
                },
            ),
            (
                "size 1 200",
                AvatarConfig {
                    size_percent: 200,
                    ..original.clone()
                },
            ),
        ] {
            let mut edited = Sent {
                avatars: sent.avatars.clone(),
                ..Default::default()
            };
            let change = request(&edited, line);
            assert_eq!(
                change,
                Edit::SetAvatarConfig {
                    avatar_id: AvatarId(1),
                    config: expected.clone()
                }
            );
            assert_eq!(edited.avatars[&AvatarId(1)], original);
            edited.commit(&change);
            assert_eq!(edited.avatars[&AvatarId(1)], expected);
            assert_eq!(edited.avatars[&AvatarId(2)], original);
            assert_eq!(edited.render, RendererConfig::default());
        }
    }

    #[test]
    fn ids_are_not_reused_and_invalid_requests_do_not_change_settings() {
        let mut sent = Sent::default();
        sent.commit(&request(&sent, "add model.vrm"));
        for line in ["size 1 49", "size 1 201", "play 99"] {
            assert!(sent.request(parse(line).unwrap().unwrap()).is_err());
        }
        assert_eq!(sent.avatars[&AvatarId(1)].size_percent, 100);
        sent.commit(&request(&sent, "remove 1"));
        let Edit::AddAvatar { avatar } = request(&sent, "add other.vrm") else {
            panic!()
        };
        assert_eq!(avatar.id, AvatarId(2));
        assert!(sent.request(ConsoleCommand::Remove { id: 1 }).is_err());
        sent.next_id = None;
        assert!(
            sent.request(ConsoleCommand::Add {
                model: Some("model.vrm".into()),
                monitor: None
            })
            .is_err()
        );
    }
}
