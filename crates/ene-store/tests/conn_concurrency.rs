//! Measures whether the single shared connection serialises concurrent reads,
//! and whether a read pool would help.
//!
//! # Decision rule, written before the first run
//!
//! A read pool is worth building only if **both** hold:
//!
//! 1. concurrent readers through the shared `Store` are materially slower than
//!    the same work run serially, and
//! 2. readers on separate connections are materially faster than readers on the
//!    shared one.
//!
//! If either fails, `Mutex<Connection>` plus WAL plus `open_secondary` is
//! enough and the pool would be speculative work.
//!
//! # What the arms are for
//!
//! - `serial` / `concurrent` — does the mutex serialise? Both run the *same*
//!   query through the *same* repository API.
//! - `separate` — the upper bound a pool could reach: one `Store` per thread,
//!   still the same query. Anything cheaper (raw SQL, a lighter query) would
//!   measure a different task and prove nothing.
//! - `cpu` — a control. If this box cannot parallelise pure CPU work, every
//!   database number above is meaningless.
//! - `one read alone` — the floor no pool can beat.
//!
//! # Result on the machine this was written on (16 cores, `--test-threads=1`)
//!
//! ```text
//! serial:               ~210 ms   (8 threads x 20 reads)
//! concurrent:           ~262 ms
//! separate connections: ~218 ms
//! one read alone:       ~1.9 ms
//! cpu speedup (8):      ~7x
//! ```
//!
//! Rule 1 fails: concurrency is not *faster* than serial, it is slightly slower,
//! so the mutex is not hiding parallelism that exists. Rule 2 fails too:
//! separate connections win ~17%, far short of the ~7x the CPU control shows
//! the box can deliver. A single recall is dominated by its own FTS5 scan and
//! RRF scoring on one core, so eight of them cannot overlap regardless of how
//! many connections exist.
//!
//! Conclusion: a read pool is not justified by measurement. Run this again
//! after the recall query changes shape, or on a database large enough that I/O
//! rather than CPU dominates.
//!
//! Run with:
//! `cargo test -p ene-store --test conn_concurrency -- --ignored --nocapture --test-threads=1`

use ene_mind::RecallMode;
use ene_store::Store;
use std::time::{Duration, Instant};

/// Panics with the error rather than `expect`, because `clippy::expect_used` is
/// workspace-deny and `allow-expect-in-tests` covers only `#[test]` bodies, not
/// module-level helpers like the ones below.
#[track_caller]
fn must<T, E: std::fmt::Display>(result: Result<T, E>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{what}: {error}"),
    }
}

/// One read that touches the connection: a recall query over the seeded rows.
fn one_read(store: &Store) {
    let repo = store.memory_repo();
    if let Err(error) =
        repo.recall_hybrid("shared", "observation number 5", RecallMode::Auto, &[], 20)
    {
        panic!("recall failed: {error}");
    }
}

/// Spawn `READS` threads, each running `body`, and time the whole join.
fn timed_threads(body: impl Fn() + Send + Sync + Clone + 'static) -> Duration {
    let start = Instant::now();
    let handles: Vec<_> = (0..READS)
        .map(|_| {
            let body = body.clone();
            std::thread::spawn(body)
        })
        .collect();
    for handle in handles {
        must(handle.join().map_err(|_| "thread panicked"), "join");
    }
    start.elapsed()
}

const READS: usize = 8;
const REPS: usize = 20;

#[test]
#[ignore = "timing measurement; run explicitly"]
fn the_shared_connection_does_not_need_a_read_pool() {
    let dir = must(tempfile::tempdir(), "temp dir");
    let path = dir.path().join("bench.sqlite3");
    let store = must(Store::open(&path), "open");
    seed(&store);

    // Warm the page cache so the first run does not pay one-time costs.
    for _ in 0..3 {
        one_read(&store);
    }

    let serial = {
        let start = Instant::now();
        for _ in 0..READS * REPS {
            one_read(&store);
        }
        start.elapsed()
    };

    let concurrent = {
        let shared = store.clone();
        timed_threads(move || {
            for _ in 0..REPS {
                one_read(&shared);
            }
        })
    };

    // The upper bound a pool could reach: one Store per thread, same query.
    let separate = timed_threads(move || {
        let own = Store::open(&path).expect("per-thread store");
        for _ in 0..REPS {
            one_read(&own);
        }
    });

    // Control: can this box parallelise pure CPU work at all?
    let cpu_serial = {
        let start = Instant::now();
        for _ in 0..READS {
            cpu_work();
        }
        start.elapsed()
    };
    let cpu_parallel = timed_threads(cpu_work);

    let one = {
        let start = Instant::now();
        one_read(&store);
        start.elapsed()
    };

    let shared_penalty = concurrent.as_secs_f64() / separate.as_secs_f64().max(f64::EPSILON);
    let cpu_speedup = cpu_serial.as_secs_f64() / cpu_parallel.as_secs_f64().max(f64::EPSILON);

    println!("serial:               {serial:?}  ({READS} threads x {REPS} reads)");
    println!("concurrent:           {concurrent:?}");
    println!("separate connections: {separate:?}");
    println!("one read alone:       {one:?}");
    println!("cpu serial:           {cpu_serial:?}");
    println!("cpu parallel(8):      {cpu_parallel:?}");
    println!("cpu speedup:          {cpu_speedup:.1}x");
    println!("shared-vs-separate penalty: {shared_penalty:.2}x");

    // The only assertion is a sanity floor: this must not hang. The measured
    // verdict lives in the module docs, not in a threshold, because a timing
    // threshold on a shared CI machine is a flake generator.
    assert!(
        concurrent < Duration::from_secs(120),
        "the measurement must terminate"
    );

    // Guard the conclusion: if the control stops parallelising, the box changed
    // and the recorded result no longer describes anything.
    assert!(
        cpu_speedup > 1.5,
        "this host no longer parallelises pure CPU work ({cpu_speedup:.1}x), \
         so the database numbers above cannot be interpreted"
    );
}

fn seed(store: &Store) {
    let connection = store.conn();
    for index in 0..500 {
        let sql = "INSERT INTO memories (id, scope, content, importance, occurred_at, \
                   recorded_at, source_context) VALUES (?1, 'shared', ?2, 5, \
                   '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 'null')";
        let id = format!("m-{index}");
        let content = format!("observation number {index} with enough text to be a real row");
        must(
            connection.execute(sql, rusqlite::params![id, content]),
            "seed",
        );
    }
}

fn cpu_work() {
    let mut acc = 0u64;
    for i in 0..20_000_000u64 {
        acc = acc.wrapping_add(i ^ (acc >> 3));
    }
    std::hint::black_box(acc);
}
