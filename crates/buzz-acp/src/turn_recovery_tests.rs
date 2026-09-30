use super::*;

async fn agent(base: &std::path::Path, lose_ack: bool, unknown: bool) -> OwnedAgent {
    let program = r#"
import json,os,sys
def send(i,value): print(json.dumps(dict(id=i,**value)),flush=True)
for raw in sys.stdin:
 q=json.loads(raw); m=q['method']; i=q.get('id')
 with open(os.environ['CAPTURE'],'a') as stream: stream.write(raw)
 if m=='initialize': send(i,{'result':{'protocolVersion':1,'agentCapabilities':{'loadSession':True},'_meta':{'axesRecovery':{'version':1}}}})
 elif m=='session/load': send(i,{'result':{}})
 elif m=='session/prompt':
  assert q['params']['_meta']['axesRecovery']['attempt']=='same-attempt'
  if os.environ['LOSE_ACK']=='true': os._exit(7)
  send(i,{'result':{'stopReason':'end_turn'}})
 elif m=='_axes/recover':
  assert q['params']=={'sessionId':'retained-session','attempt':'same-attempt'}
  if os.environ['UNKNOWN']=='true': send(i,{'error':{'code':-32603,'message':'native dispatch outcome unknown'}})
  else: send(i,{'result':{'stopReason':'end_turn'}})
 else: raise RuntimeError(m)
"#;
    let mut acp = AcpClient::spawn(
        "python3",
        &["-u".into(), "-c".into(), program.into()],
        &[
            (
                "CAPTURE".into(),
                base.join("wire.jsonl").display().to_string(),
            ),
            ("LOSE_ACK".into(), lose_ack.to_string()),
            ("UNKNOWN".into(), unknown.to_string()),
        ],
        false,
    )
    .await
    .unwrap();
    acp.initialize().await.unwrap();
    OwnedAgent {
        index: 0,
        acp,
        state: SessionState::open(&base.join("checkpoints"), 0, "same-identity").unwrap(),
        model_capabilities: None,
        desired_model: None,
        model_overridden: false,
        desired_model_request_id: None,
        desired_model_pending_ack: false,
        startup_effort: None,
        agent_name: "recovery-test".into(),
        goose_system_prompt_supported: None,
        protocol_version: 1,
    }
}

#[tokio::test]
async fn lost_ack_recovers_on_quiet_restart_without_new_prompt_or_reminder_delivery() {
    for unknown in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path();
        let mut worker = agent(base, true, false).await;
        worker.state.heartbeat_session = Some("retained-session".into());
        let ctx = Arc::new(tests::make_prompt_context_no_owner());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let reminder = ("reminder-id".into(), "a".repeat(64));
        run_prompt_task(
            worker,
            None,
            Some(PrivatePrompt {
                text: "original private work".into(),
                source: PromptSource::Reminder,
                reminder: Some(reminder.clone()),
            }),
            Arc::clone(&ctx),
            tx.clone(),
            None,
            "same-attempt".into(),
        )
        .await;
        let mut result = rx.recv().await.unwrap();
        assert!(result.agent.state.turn_active);
        assert!(result.agent.state.pending_turn.is_some());
        assert!(result.batch.is_none());
        result.agent.acp.shutdown().await;
        drop(result);
        let restarted = agent(base, false, unknown).await;
        crate::turn_recovery::run(restarted, Arc::clone(&ctx), tx).await;
        let mut result = rx.recv().await.unwrap();
        if unknown {
            assert!(result.agent.state.pending_turn.is_some());
            assert!(result.agent.state.turn_active);
            assert!(result.agent.state.recovery_held);
            assert!(result.agent.state.recovered_reminders.is_empty());
            assert!(matches!(
                result.outcome,
                PromptOutcome::Error(AcpError::AgentError { code: -32073, .. })
            ));
            let saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(base.join("checkpoints/0.json")).unwrap())
                    .unwrap();
            assert_eq!(saved["state"]["recovery_held"], true);
            assert_eq!(saved["state"]["pending_turn"]["attempt"], "same-attempt");
        } else {
            assert!(result.agent.state.pending_turn.is_none());
            assert!(!result.agent.state.turn_active);
            assert_eq!(result.agent.state.recovered_reminders, vec![reminder]);
        }
        result.agent.acp.shutdown().await;
        let calls: Vec<serde_json::Value> = std::fs::read_to_string(base.join("wire.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call["method"] == "session/prompt")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call["method"] == "session/load")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call["method"] == "_axes/recover")
                .count(),
            1
        );
    }
}

#[test]
fn recovered_channel_delivery_commits_exact_scope_and_events_atomically() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let scope = SessionScope::Conversation {
        channel_id: Uuid::new_v4(),
    };
    let mut state = SessionState::open(&directory, 0, "identity").unwrap();
    state.sessions.insert(scope.clone(), "retained".into());
    state.turn_active = true;
    state.pending_turn = Some(crate::turn_recovery::Pending {
        attempt: "attempt".into(),
        session: "retained".into(),
        source: PromptSource::Channel(scope.clone()),
        delivered: vec!["original-event".into()],
        standing: true,
        reminder: None,
    });
    state.checkpoint().unwrap();
    drop(state);
    let mut state = SessionState::open(&directory, 0, "identity").unwrap();
    state.complete_recovery(true).unwrap();
    drop(state);
    let state = SessionState::open(&directory, 0, "identity").unwrap();
    assert!(!state.turn_active);
    assert!(state.pending_turn.is_none());
    assert_eq!(state.sessions[&scope], "retained");
    assert!(state.deliveries[&scope].standing_context_sent);
    assert!(state.deliveries[&scope]
        .delivered_event_ids
        .contains("original-event"));
}
