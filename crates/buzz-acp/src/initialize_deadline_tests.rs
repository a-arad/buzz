//! The configured `initialize` deadline governs pool startup and respawn.
use super::*;
use std::path::Path;

/// Records its PID, waits `INITIALIZE_DELAY` seconds, then answers `initialize`.
const SLOW_ADAPTER: &str = r#"
import os
open(os.environ['ADAPTER_PID_FILE'],'w').write(str(os.getpid()))
import json,sys,time
time.sleep(float(os.environ['INITIALIZE_DELAY']))
request=json.loads(sys.stdin.readline())
assert request['method']=='initialize', request
print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':{'protocolVersion':1,'agentCapabilities':{}}}),flush=True)
sys.stdin.read()
"#;

/// Long enough that the adapter never answers within a test.
const NEVER: f64 = 3600.0;

fn startup(directory: &Path, delay_secs: f64, initialize_timeout: Duration) -> PoolStartup {
    PoolStartup {
        session_identity: "initialize-deadline".into(),
        agents: 1,
        command: "python3".into(),
        args: vec!["-u".into(), "-c".into(), SLOW_ADAPTER.into()],
        extra_env: vec![
            (
                "ADAPTER_PID_FILE".into(),
                directory.join("pid").display().to_string(),
            ),
            ("INITIALIZE_DELAY".into(), delay_secs.to_string()),
        ],
        has_generated_codex_config: false,
        initialize_timeout,
        model: None,
        effort_level: None,
        observer: None,
    }
}

fn assert_reaped(directory: &Path) {
    use nix::{sys::signal::kill, unistd::Pid};
    let pid: i32 = std::fs::read_to_string(directory.join("pid"))
        .expect("adapter recorded its pid before the deadline")
        .parse()
        .unwrap();
    assert!(
        kill(Pid::from_raw(pid), None).is_err(),
        "adapter {pid} must be killed and reaped after a missed deadline"
    );
}

async fn spawn_and_init_from(startup: &PoolStartup) -> Result<(AcpClient, u32, String)> {
    spawn_and_init(
        &startup.command,
        &startup.args,
        &startup.extra_env,
        false,
        startup.initialize_timeout,
        0,
        None,
    )
    .await
}

#[tokio::test]
async fn pool_startup_reaps_adapter_that_misses_configured_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let startup = startup(directory.path(), NEVER, Duration::from_secs(5));
    let started = std::time::Instant::now();
    let error = initialize_agent_pool(&startup, None)
        .await
        .err()
        .expect("an adapter that never answers must not start");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the configured 5 s deadline, not the 60 s default, must govern: {:?}",
        started.elapsed()
    );
    assert!(error.to_string().contains("failed to start"), "{error}");
    assert_reaped(directory.path());
}

#[tokio::test]
async fn respawn_reaps_adapter_that_misses_configured_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();
    let error = spawn_and_init_from(&startup(directory.path(), NEVER, Duration::from_secs(5)))
        .await
        .err()
        .expect("an adapter that never answers must not respawn");
    assert!(started.elapsed() < Duration::from_secs(30));
    assert!(
        error.to_string().contains("did not respond within 5s"),
        "{error}"
    );
    assert_reaped(directory.path());
}

#[tokio::test]
async fn adapter_exit_before_initialize_fails_without_waiting_for_long_deadline() {
    // A fail-closed adapter check exits before `initialize`; a long deadline
    // must not turn that prompt startup failure into a wait.
    let directory = tempfile::tempdir().unwrap();
    let mut startup = startup(directory.path(), 0.0, Duration::from_secs(3600));
    startup.args = vec!["-c".into(), "import sys; sys.exit(3)".into()];
    let started = std::time::Instant::now();
    let error = initialize_agent_pool(&startup, None)
        .await
        .err()
        .expect("an adapter that exits before initialize must not start");
    assert!(started.elapsed() < Duration::from_secs(30));
    assert!(error.to_string().contains("failed to start"), "{error}");
}

/// The incident case: release-tree verification ran ~57 s before `initialize`.
#[tokio::test]
#[ignore = "takes ~65 s; run with --ignored to prove startup outlives the fixed 60 s RPC limit"]
async fn pool_startup_outlives_fixed_rpc_timeout_when_configured() {
    let directory = tempfile::tempdir().unwrap();
    let startup = startup(directory.path(), 65.0, Duration::from_secs(90));
    let mut pool = initialize_agent_pool(&startup, None).await.unwrap();
    assert_eq!(pool.agents_mut().iter().flatten().count(), 1);
    shutdown_agent_pool(&mut pool).await;
}
