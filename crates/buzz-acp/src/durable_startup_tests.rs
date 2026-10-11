//! Exercise startup failures through real ACP subprocesses and durable stores.
use super::*;

const TEST: &str = "durable_startup_tests::failed_slot_cannot_transfer_durable_ownership";
const CASE: &str = "BUZZ_TEST_DURABLE_STARTUP_CASE";

#[tokio::test]
async fn failed_slot_cannot_transfer_durable_ownership() {
    let Ok(case) = std::env::var(CASE) else {
        // A separate test process owns the session-store environment. Parallel
        // library tests must never observe this fixture's production env var.
        for case in ["durable-failure", "ephemeral-failure", "durable-success"] {
            let directory = tempfile::tempdir().unwrap();
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", TEST, "--nocapture"])
                .env(CASE, case)
                .env("BUZZ_TEST_STARTUP_DIRECTORY", directory.path());
            if case.starts_with("durable") {
                command.env("BUZZ_ACP_SESSION_STORE", directory.path().join("sessions"));
            } else {
                command.env_remove("BUZZ_ACP_SESSION_STORE");
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    };
    let directory = std::path::PathBuf::from(std::env::var("BUZZ_TEST_STARTUP_DIRECTORY").unwrap());
    let store = directory.join("sessions");
    let scope = scope::SessionScope::Conversation {
        channel_id: Uuid::new_v4(),
    };
    for index in 0..2 {
        let mut state = SessionState::open(&store, index, "same-identity").unwrap();
        state
            .sessions
            .insert(scope.clone(), format!("native-{index}"));
        state.mark_scope_delivery_success(scope.clone(), true, [format!("receipt-{index}")], []);
        state.checkpoint().unwrap();
    }
    let before: Vec<_> = (0..2)
        .map(|index| std::fs::read(store.join(format!("{index}.json"))).unwrap())
        .collect();
    let script = r#"
import json,os,pathlib,sys
marker=pathlib.Path(os.environ['STARTUP_MARKER'])
try:
    marker.open('x').close()
    first=True
except FileExistsError:
    first=False
for line in sys.stdin:
    request=json.loads(line)
    assert request['method']=='initialize', 'startup must never prompt or load'
    response={'jsonrpc':'2.0','id':request['id']}
    if first and os.environ['FAIL_FIRST']=='true':
        response['error']={'code':-32603,'message':'injected initialization failure'}
    else:
        response['result']={'protocolVersion':1,'agentCapabilities':{'loadSession':True}}
    print(json.dumps(response),flush=True)
"#;
    let startup = PoolStartup {
        session_identity: "same-identity".into(),
        agents: 2,
        command: "python3".into(),
        args: vec!["-u".into(), "-c".into(), script.into()],
        extra_env: vec![
            (
                "STARTUP_MARKER".into(),
                directory.join("first").display().to_string(),
            ),
            ("FAIL_FIRST".into(), (case != "durable-success").to_string()),
        ],
        has_generated_codex_config: false,
        model: None,
        effort_level: None,
        observer: None,
    };
    let result = initialize_agent_pool(&startup, None).await;
    if case == "durable-failure" {
        assert!(
            result.is_err(),
            "partial startup must not admit durable work"
        );
        for (index, original) in before.iter().enumerate() {
            assert_eq!(
                &std::fs::read(store.join(format!("{index}.json"))).unwrap(),
                original,
                "failed startup must not invent a newer owner or rewrite receipts"
            );
            // No pool retains the failed attempt's checkpoint locks.
            let state = SessionState::open(&store, index, "same-identity").unwrap();
            assert_eq!(state.sessions[&scope], format!("native-{index}"));
        }
    } else {
        let mut pool = result.unwrap();
        let live = pool.agents_mut().iter().flatten().count();
        assert_eq!(live, if case == "durable-success" { 2 } else { 1 });
        if case == "durable-success" {
            let worker = pool.try_claim(Some(&scope)).unwrap();
            assert_eq!(worker.state.sessions[&scope], "native-0");
            assert!(worker.state.deliveries[&scope]
                .delivered_event_ids
                .contains("receipt-0"));
            pool.return_agent(worker);
        }
        shutdown_agent_pool(&mut pool).await;
    }
}
