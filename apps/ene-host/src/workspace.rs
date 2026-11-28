use crate::{
    actor::{Host, id},
    approval::Operation,
    error::{conflict, denied},
    model::truncate,
};
use anyhow::{Context, Result, ensure};
use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _, OpenOptionsSyncExt as _};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use ene_core::policy::{Action, Effect, Target};
use rig_core::{completion::ToolDefinition, message::ToolName};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    io::{Read as _, Write as _},
    path::{Component, Path, PathBuf},
    time::Duration,
};

const MAX_FILE: u64 = 1024 * 1024;
const TOOLS: &[&str] = &["read", "search", "edit", "write", "shell"];

pub(crate) struct Workspace {
    root: PathBuf,
    dir: Dir,
    tasks: BTreeMap<String, Vec<String>>,
    revision: String,
    writes: std::sync::Arc<parking_lot::Mutex<()>>,
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Read {
        path: PathBuf,
        start_line: Option<usize>,
        lines: Option<usize>,
    },
    Search {
        query: String,
        path: Option<PathBuf>,
    },
    Edit {
        path: PathBuf,
        old: String,
        new: String,
    },
    Write {
        path: PathBuf,
        text: String,
    },
    Shell {
        task: String,
    },
}

impl Workspace {
    pub(crate) fn from_env() -> Result<Option<Self>> {
        let Some(root) = std::env::var_os("ENE_WORKSPACE") else {
            return Ok(None);
        };
        let root = Path::new(&root).canonicalize()?;
        let task_file = std::env::var_os("ENE_WORKSPACE_TASKS");
        let tasks = if let Some(path) = task_file {
            let path = Path::new(&path).canonicalize()?;
            ensure!(
                !path.starts_with(&root),
                "workspace task configuration must be outside the work directory"
            );
            let mut json = Vec::new();
            std::fs::File::open(path)?
                .take(65_537)
                .read_to_end(&mut json)?;
            ensure!(
                json.len() <= 65_536,
                "workspace task configuration is too large"
            );
            serde_json::from_slice(&json)?
        } else {
            BTreeMap::new()
        };
        Ok(Some(Self::new(&root, tasks)?))
    }

    fn new(root: &Path, tasks: BTreeMap<String, Vec<String>>) -> Result<Self> {
        let root = root.canonicalize()?;
        let dir = Dir::open_ambient_dir(&root, ambient_authority())?;
        if tasks.len() > 32
            || tasks.iter().any(|(name, argv)| {
                name.is_empty()
                    || name.len() > 80
                    || argv.is_empty()
                    || argv[0].trim().is_empty()
                    || argv.len() > 48
                    || argv
                        .iter()
                        .any(|argument| argument.len() > 1024 || argument.contains('\0'))
            })
        {
            anyhow::bail!("invalid workspace task configuration");
        }
        let revision = crate::store::fingerprint(&(&root, &tasks))?;
        Ok(Self {
            root,
            dir,
            tasks,
            revision,
            writes: Default::default(),
        })
    }

    pub(crate) fn handles(name: &str) -> bool {
        TOOLS.contains(&name)
    }

    pub(crate) fn definitions(&self) -> Result<Vec<ToolDefinition>> {
        let path =
            json!({"type":"string","description":"Relative path under the configured workspace"});
        let text = json!({"type":"string","maxLength":32768});
        let mut specs = vec![
            (
                "read",
                "Read lines from a UTF-8 file",
                json!({"path":path,"start_line":{"type":"integer","minimum":1},"lines":{"type":"integer","minimum":1,"maximum":200}}),
                vec!["path"],
            ),
            (
                "search",
                "Find a literal string in workspace text files",
                json!({"query":{"type":"string","minLength":1,"maxLength":512},"path":path}),
                vec!["query"],
            ),
            (
                "edit",
                "Replace one exact unique text match in an existing file",
                json!({"path":path,"old":text,"new":text}),
                vec!["path", "old", "new"],
            ),
            (
                "write",
                "Create a UTF-8 file in an existing directory",
                json!({"path":path,"text":text}),
                vec!["path", "text"],
            ),
        ];
        if !self.tasks.is_empty() {
            specs.push((
                "shell",
                "Run a user-configured project command and report its exit status and output",
                json!({"task":{"type":"string","enum":self.tasks.keys().collect::<Vec<_>>()}}),
                vec!["task"],
            ));
        }
        specs.into_iter().map(|(name, description, properties, required)| {
            Ok(ToolDefinition::new(ToolName::new(name)?, description,
                json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})))
        }).collect()
    }

    pub(crate) async fn invoke(
        &self,
        host: &Host,
        operation: &Operation,
        name: &str,
        arguments: Map<String, Value>,
    ) -> Result<String> {
        ensure!(
            !arguments.contains_key("operation"),
            "invalid workspace arguments"
        );
        let mut request = arguments.clone();
        request.insert("operation".into(), json!(name));
        let request: Request = serde_json::from_value(Value::Object(request))?;
        let effect = match request {
            Request::Read { .. } | Request::Search { .. } => Effect::Read,
            Request::Edit { .. } | Request::Write { .. } => Effect::Write,
            Request::Shell { .. } => Effect::Execute,
        };
        let mut argv = None;
        let mut arguments = arguments;
        if let Request::Shell { task } = &request {
            let recipe = self
                .tasks
                .get(task)
                .context("workspace task not configured")?;
            arguments.insert("argv".into(), json!(recipe));
            argv = Some(recipe);
        }
        let previous = if let Request::Edit { path, .. } = &request {
            let dir = self.dir.try_clone()?;
            let path = path.clone();
            let current = tokio::task::spawn_blocking(move || read(&dir, &path)).await??;
            let hash = crate::store::fingerprint(&current)?;
            arguments.insert("previous_sha256".into(), json!(hash));
            Some(current)
        } else {
            None
        };
        let action = Action::new(
            Target {
                receiver: format!("workspace/{}", self.root.display()),
                revision: self.revision.clone(),
                operation: format!("work.{name}"),
                effect,
                device: None,
            },
            arguments,
        )?;
        let digest = host.authorize(operation, action.clone()).await?;
        let companion = operation.companion.clone();
        let run = operation.run.clone();
        host.call(move |actor| {
            Box::pin(async move {
                actor.check_operation(&companion, &run, &action)?;
                action.verify(&digest)
            })
        })
        .await?;
        let cancel = &operation.cancel;
        if let Some(argv) = argv {
            let (program, args) = argv.split_first().context("workspace task is empty")?;
            return tokio::select! {
                () = cancel.cancelled() => Err(denied("work stopped")),
                result = tokio::time::timeout(Duration::from_secs(60), crate::process::run_task(program, args, &self.root)) => result?,
            };
        }
        let dir = self.dir.try_clone()?;
        let root = self.root.clone();
        let cancel = cancel.clone();
        let writes = self.writes.clone();
        tokio::task::spawn_blocking(move || {
            let _write = matches!(effect, Effect::Write).then(|| writes.lock());
            ensure!(!cancel.is_cancelled(), "work stopped");
            files(&dir, &root, request, previous.as_deref(), &cancel)
        })
        .await?
    }
}

fn safe_path(dir: &Dir, path: &Path, missing_last: bool) -> Result<()> {
    ensure!(!path.as_os_str().is_empty(), "path missing");
    let mut parts = path.components().peekable();
    let mut walked = PathBuf::new();
    while let Some(component) = parts.next() {
        let Component::Normal(name) = component else {
            return Err(denied("use a workspace relative path"));
        };
        let name_text = name.to_string_lossy().to_ascii_lowercase();
        ensure!(
            name_text != ".git"
                && !name_text.starts_with(".env")
                && !name_text.starts_with(".ene-")
                && !name_text.ends_with(".pem")
                && !name_text.ends_with(".key"),
            "protected project path"
        );
        walked.push(name);
        match dir.symlink_metadata(&walked) {
            Ok(meta) => ensure!(!meta.is_symlink(), "workspace symlinks are unavailable"),
            Err(error)
                if missing_last
                    && parts.peek().is_none()
                    && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn read(dir: &Dir, path: &Path) -> Result<String> {
    safe_path(dir, path, false)?;
    ensure!(dir.metadata(path)?.is_file(), "regular text file required");
    let (dir, name) = open_parent(dir, path)?;
    let mut data = String::new();
    let mut options = OpenOptions::new();
    let file = dir.open_with(
        &name,
        options.read(true).follow(FollowSymlinks::No).nonblock(true),
    )?;
    ensure!(file.metadata()?.is_file(), "regular text file required");
    file.take(MAX_FILE + 1).read_to_string(&mut data)?;
    ensure!(
        data.len() as u64 <= MAX_FILE,
        "workspace file exceeds the read limit"
    );
    Ok(data)
}

fn open_parent(dir: &Dir, path: &Path) -> Result<(Dir, std::ffi::OsString)> {
    let mut cursor = dir.try_clone()?;
    if let Some(parent) = path.parent() {
        for component in parent.components() {
            cursor = cursor.open_dir_nofollow(component.as_os_str())?;
        }
    }
    Ok((
        cursor,
        path.file_name()
            .context("workspace file name missing")?
            .to_owned(),
    ))
}

fn files(
    dir: &Dir,
    root: &Path,
    request: Request,
    previous: Option<&str>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String> {
    match request {
        Request::Read {
            path,
            start_line,
            lines,
        } => {
            let start = start_line.unwrap_or(1);
            let lines = lines.unwrap_or(120);
            ensure!(
                start > 0 && (1..=200).contains(&lines),
                "invalid line range"
            );
            let text = read(dir, &path)?;
            let mut selected = Vec::new();
            let mut size = 0;
            for (index, line) in text.lines().enumerate().skip(start - 1).take(lines) {
                let line = format!("{}: {}", index + 1, truncate(line, 1000));
                size += serde_json::to_vec(&line)?.len() + 1;
                if size > 9_000 {
                    break;
                }
                selected.push(line);
            }
            Ok(json!({"path":path,"lines":selected}).to_string())
        }
        Request::Search { query, path } => {
            ensure!(
                !query.is_empty() && query.len() <= 512,
                "invalid search query"
            );
            if let Some(path) = &path {
                safe_path(dir, path, false)?;
            }
            let base = path.map_or_else(|| root.to_owned(), |path| root.join(path));
            let mut matches = Vec::new();
            for entry in ignore::WalkBuilder::new(base)
                .follow_links(false)
                .max_filesize(Some(MAX_FILE))
                .build()
                .take(10_000)
            {
                ensure!(!cancel.is_cancelled(), "work stopped");
                let Ok(entry) = entry else { continue };
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    continue;
                }
                let path = entry.path().strip_prefix(root)?;
                let Ok(text) = read(dir, path) else { continue };
                for (line, text) in text
                    .lines()
                    .enumerate()
                    .filter(|(_, text)| text.contains(&query))
                {
                    matches.push(json!({"path":path,"line":line+1,"text":truncate(text,240)}));
                    if matches.len() >= 30 {
                        return Ok(json!({"matches":matches,"limit_reached":true}).to_string());
                    }
                }
            }
            Ok(json!({"matches":matches}).to_string())
        }
        Request::Edit { path, old, new } => {
            ensure!(!old.is_empty(), "edit requires an exact search string");
            let previous = previous.context("approved file version missing")?;
            ensure!(
                previous.match_indices(&old).take(2).count() == 1,
                "edit must match exactly once"
            );
            let text = previous.replacen(&old, &new, 1);
            save(dir, &path, &text, Some(previous), cancel)?;
            Ok(json!({"edited":path}).to_string())
        }
        Request::Write { path, text } => {
            safe_path(dir, &path, true)?;
            save(dir, &path, &text, None, cancel)?;
            Ok(json!({"created":path}).to_string())
        }
        Request::Shell { .. } => anyhow::bail!("task execution uses the process runner"),
    }
}

fn save(
    dir: &Dir,
    destination: &Path,
    text: &str,
    previous: Option<&str>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    ensure!(
        text.len() as u64 <= MAX_FILE,
        "workspace file exceeds the write limit"
    );
    let (dir, destination) = open_parent(dir, destination)?;
    let destination = Path::new(&destination);
    let temp = PathBuf::from(format!(".ene-{}", id()));
    let mut options = OpenOptions::new();
    let mut file = dir.open_with(&temp, options.write(true).create_new(true))?;
    let outcome = (|| {
        file.write_all(text.as_bytes())?;
        if previous.is_some() {
            file.set_permissions(dir.metadata(destination)?.permissions())?;
        }
        file.sync_all()?;
        drop(file);
        ensure!(!cancel.is_cancelled(), "work stopped");
        if let Some(previous) = previous {
            if read(&dir, destination)? != previous {
                return Err(conflict("workspace file changed"));
            }
            dir.rename(&temp, &dir, destination)?;
        } else {
            dir.hard_link(&temp, &dir, destination)?;
        }
        Ok(())
    })();
    match dir.remove_file(&temp) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(%error, "workspace temporary file could not be removed"),
    }
    outcome
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use ene_rpc::{self as w, event::Payload};

    #[tokio::test]
    async fn workspace_invocations_follow_the_card_and_approved_file_version() -> Result<()> {
        let home = tempfile::tempdir()?;
        let project = home.path().join("project");
        let state = home.path().join("state");
        std::fs::create_dir(&project)?;
        std::fs::create_dir_all(state.join("conversations"))?;
        let workspace = std::sync::Arc::new(Workspace::new(
            &project,
            BTreeMap::from([("file-check".into(), vec!["cat".into(), "notes.txt".into()])]),
        )?);
        assert_eq!(workspace.definitions()?.len(), TOOLS.len());
        let (host, actor) = crate::actor::Actor::start(state).await?;
        let mut events = host
            .call(|actor| Box::pin(async { Ok(actor.events.subscribe()) }))
            .await?;
        for (index, (name, arguments, expected)) in [
            (
                "write",
                json!({"path":"notes.txt","text":"one\ntwo\n"}),
                "created",
            ),
            (
                "read",
                json!({"path":"notes.txt","start_line":2,"lines":1}),
                "2: two",
            ),
            ("search", json!({"query":"two"}), "notes.txt"),
            (
                "edit",
                json!({"path":"notes.txt","old":"two","new":"three"}),
                "edited",
            ),
            ("shell", json!({"task":"file-check"}), "three"),
            (
                "edit",
                json!({"path":"notes.txt","old":"three","new":"four"}),
                "",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let task = tokio::spawn({
                let workspace = workspace.clone();
                let host = host.clone();
                async move {
                    let operation = Operation {
                        companion: "ene".into(),
                        run: String::new(),
                        brief: "Test project tools".into(),
                        cancel: host.shutdown.child_token(),
                    };
                    workspace
                        .invoke(&host, &operation, name, serde_json::from_value(arguments)?)
                        .await
                }
            });
            let card = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Some(Payload::Approval(card)) = events.recv().await?.payload {
                        return Ok::<_, anyhow::Error>(card);
                    }
                }
            })
            .await??;
            assert_eq!(card.operation, format!("work.{name}"));
            assert_eq!(
                card.arguments_json.contains("previous_sha256"),
                name == "edit"
            );
            if name == "shell" {
                assert!(card.card_only);
                assert!(card.arguments_json.contains("cat"));
            }
            if index == 5 {
                std::fs::write(project.join("notes.txt"), "three with manual changes")?;
            }
            host.call(move |actor| {
                Box::pin(
                    actor.answer(w::ApprovalAnswer {
                        mutation: Some(w::Mutation {
                            request_key: id(),
                            ..Default::default()
                        })
                        .into(),
                        id: card.id,
                        digest: card.digest,
                        choice: w::approval_answer::Choice::Allow.into(),
                        ..Default::default()
                    }),
                )
            })
            .await?;
            let result = task.await?;
            if expected.is_empty() {
                assert!(result.is_err());
            } else {
                assert!(result?.contains(expected));
            }
        }
        assert_eq!(
            std::fs::read_to_string(project.join("notes.txt"))?,
            "three with manual changes"
        );
        std::os::unix::fs::symlink("/etc/hosts", project.join("outside"))?;
        assert!(read(&workspace.dir, Path::new("outside")).is_err());
        assert!(read(&workspace.dir, Path::new("../state/ene.sqlite3")).is_err());
        host.shutdown.cancel();
        actor.await?;
        Ok(())
    }
}
