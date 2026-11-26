//! Runtime tests: span propagation and the bounded actor queue.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

#[tokio::test]
async fn discarded_replies_leave_bounded_nonblocking_actor_jobs() {
    let state = tempfile::tempdir().unwrap();
    let runtime = Runtime::start(state.path()).await.unwrap();
    let (entered, started) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let blocked = runtime.read(move |_| {
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
        Ok(())
    });
    started.recv_timeout(Duration::from_secs(10)).unwrap();
    let completed = Arc::new(AtomicUsize::new(0));
    for _ in 0..QUEUE - 1 {
        let completed = completed.clone();
        drop(runtime.read(move |_| {
            completed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }));
    }
    let last = runtime.read({
        let completed = completed.clone();
        move |_| {
            completed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    });
    assert!(matches!(
        runtime
            .call::<(), _>(|_| panic!("a full queue must reject this job"))
            .await,
        Err(HostError::Exhausted(_))
    ));
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    blocked.await.unwrap();
    last.await.unwrap();
    assert_eq!(completed.load(Ordering::SeqCst), QUEUE);
    runtime.shutdown().await.unwrap();
    assert!(matches!(
        runtime
            .read::<(), _>(|_| panic!("a closed queue must reject this job"))
            .await,
        Err(HostError::Exhausted(_))
    ));
}

#[tokio::test]
async fn storage_errors_with_discarded_replies_still_stop_the_host() {
    for mutation in [false, true] {
        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        runtime
            .read(|app| {
                app.world()
                    .resource::<Store>()
                    .conn()
                    .execute_batch("DROP TABLE clients")
                    .map_err(ene_store::StoreError::from)?;
                Ok(())
            })
            .await
            .unwrap();
        let broken = |app: &mut App| {
            app.world()
                .resource::<Store>()
                .client_repo()
                .client("c1")
                .map(|_| ())
                .map_err(HostError::from)
        };
        if mutation {
            drop(runtime.call(broken));
        } else {
            drop(runtime.read(broken));
        }
        assert!(matches!(
            runtime
                .read::<(), _>(|_| panic!("a failed host must refuse work"))
                .await,
            Err(HostError::Unavailable(_))
        ));
        assert!(matches!(
            runtime.shutdown().await,
            Err(HostError::Unavailable(_))
        ));
    }
}

#[tokio::test]
async fn fresh_runtime_seeds_ene_once_and_does_not_resurrect_after_delete() {
    let state = tempfile::tempdir().unwrap();
    let runtime = Runtime::start(state.path()).await.unwrap();
    let snap = runtime
        .inspect(|services| SnapshotReader::from(services).get())
        .await
        .unwrap();
    assert!(
        snap.companions
            .iter()
            .any(|c| c.id == "ene" && c.name == "ene"),
        "fresh host must include seeded ene companion"
    );
    runtime
        .call(|app| {
            super::mind::delete(app.world_mut(), "ene")?;
            Ok(())
        })
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();

    let restarted = Runtime::start(state.path()).await.unwrap();
    let snap = restarted
        .inspect(|services| SnapshotReader::from(services).get())
        .await
        .unwrap();
    assert!(
        snap.companions.is_empty(),
        "deleted initial companion must stay deleted after restart"
    );
    restarted.shutdown().await.unwrap();
}

/// Installs a real subscriber so spans have ids to compare.
///
/// The default must be global, not thread-local: the actor runs jobs on its
/// own thread, where `with_default` is not in effect.
fn install_subscriber() {
    static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INSTALLED.get_or_init(|| {
        // Another test may already have installed one; the assertion below
        // holds under any registry that assigns span ids.
        let _installed = tracing::subscriber::set_global_default(tracing_subscriber::registry());
    });
}

#[tokio::test]
async fn a_job_runs_inside_the_callers_span() {
    install_subscriber();
    let state = tempfile::tempdir().unwrap();
    let runtime = Runtime::start(state.path()).await.unwrap();

    let caller = tracing::info_span!("rpc-caller");
    let observed = Arc::new(parking_lot::Mutex::new(None));
    let slot = Arc::clone(&observed);
    caller
        .in_scope(|| {
            runtime.read(move |_| {
                // The span the actor thread is running this job inside.
                *slot.lock() = tracing::Span::current().id();
                Ok(())
            })
        })
        .await
        .unwrap();

    let seen = observed.lock().clone();
    assert!(caller.id().is_some(), "the caller span must be enabled");
    assert_eq!(
        seen,
        caller.id(),
        "the job must run inside the caller's span, not a fresh one"
    );
    runtime.shutdown().await.unwrap();
}
