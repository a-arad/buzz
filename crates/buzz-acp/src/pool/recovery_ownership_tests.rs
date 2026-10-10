#[tokio::test]
async fn restart_retains_newest_thread_owner_and_receipts() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("sessions");
    let scope = SessionScope::Thread { channel_id: Uuid::new_v4(), root_event_id: "a".repeat(64) };
    // Both checkpoints exist when the former worker is interrupted before
    // returning. Restart must retain the newest upstream ownership generation.
    for index in 0..2 {
        let mut state = SessionState::open(&store, index, "identity").unwrap();
        state.sessions.insert(scope.clone(), format!("native-{index}"));
        state.set_scope_owner_generation(scope.clone(), (index + 1) as u64);
        state.mark_scope_delivery_success(scope.clone(), true, [format!("receipt-{index}")], []);
        state.checkpoint().unwrap();
    }
    let mut agents = Vec::new();
    for index in 0..2 {
        let mut agent = idle_agent_with_session(index, scope.clone()).await;
        agent.state = SessionState::open(&store, index, "identity").unwrap();
        agents.push(Some(agent));
    }
    let mut pool = AgentPool::from_slots(agents);
    let agent = pool.try_claim(Some(&scope)).unwrap();
    assert_eq!(agent.index, 1);
    assert_eq!(agent.state.sessions[&scope], "native-1");
    assert!(agent.state.deliveries[&scope].delivered_event_ids.contains(&"receipt-1".to_owned()));
    pool.return_agent(agent);
    let old = pool.agents[0].take().unwrap();
    pool.return_agent(old);
    assert!(!pool.agents[0].as_ref().unwrap().state.sessions.contains_key(&scope));
    assert_eq!(pool.try_claim(Some(&scope)).unwrap().index, 1);
    for agent in pool.agents.iter_mut().flatten() { agent.acp.shutdown().await; }
}

#[tokio::test]
async fn legacy_checkpoint_and_respawn_retain_addressed_conversation() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("sessions");
    let scope = conv(Uuid::new_v4());
    let mut state = SessionState::open(&store, 0, "identity").unwrap();
    state.sessions.insert(scope.clone(), "original-native".into());
    state.checkpoint().unwrap();
    drop(state);
    let mut agent = idle_agent_with_session(0, scope.clone()).await;
    agent.state = SessionState::open(&store, 0, "identity").unwrap();
    let mut pool = AgentPool::from_slots(vec![Some(agent)]);
    let mut prior = pool.try_claim(Some(&scope)).unwrap();
    prior.acp.shutdown().await;
    drop(prior);
    let mut respawned = idle_agent_with_session(0, scope.clone()).await;
    respawned.state = SessionState::open(&store, 0, "identity").unwrap();
    pool.return_restored_agent(respawned);
    let mut restored = pool.try_claim(Some(&scope)).unwrap();
    assert_eq!(restored.state.sessions[&scope], "original-native");
    assert!(restored.state.pending_loads.contains("original-native"));
    restored.acp.shutdown().await;
}

#[tokio::test]
async fn legacy_parallel_conversations_keep_affinity_until_first_dispatch() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("sessions");
    let scope = conv(Uuid::new_v4());
    let mut agents = Vec::new();
    for index in 0..2 {
        let mut agent = idle_agent_with_session(index, scope.clone()).await;
        agent.state = SessionState::open(&store, index, "identity").unwrap();
        agent.state.sessions.insert(scope.clone(), format!("legacy-{index}"));
        agent.state.checkpoint().unwrap();
        agents.push(Some(agent));
    }
    let mut pool = AgentPool::from_slots(agents);
    // This matches the installed pre-generation first-idle affinity. Both
    // references survive startup; the next actual dispatch selects the owner.
    let mut first = pool.try_claim(Some(&scope)).unwrap();
    assert_eq!(first.state.sessions[&scope], "legacy-0");
    assert_eq!(pool.agents[1].as_ref().unwrap().state.sessions[&scope], "legacy-1");
    let generation = pool.record_scope_owner(scope.clone(), first.index);
    first.state.set_scope_owner_generation(scope.clone(), generation);
    pool.return_agent(first);
    let mut stale = pool.agents[1].take().unwrap();
    stale.acp.shutdown().await;
    pool.return_agent(stale);
    let mut selected = pool.try_claim(Some(&scope)).unwrap();
    assert_eq!(selected.state.sessions[&scope], "legacy-0");
    assert!(!pool.agents[1].as_ref().unwrap().state.sessions.contains_key(&scope));
    selected.acp.shutdown().await;
}

#[test]
fn installed_checkpoint_without_upstream_fields_restores_delivery_history() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("sessions");
    let scope = conv(Uuid::new_v4());
    let mut state = SessionState::open(&store, 0, "identity").unwrap();
    state.sessions.insert(scope.clone(), "installed-conversation".into());
    state.mark_scope_delivery_success(scope.clone(), true, ["retained-receipt".into()], []);
    state.checkpoint().unwrap();
    drop(state);
    // The deployed 9c219f4b checkpoint predates ownership generations and
    // hydrated roots. Exercise its persisted shape through the real loader.
    let path = store.join("0.json");
    let mut legacy: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    legacy["state"].as_object_mut().unwrap().remove("scope_owner_generations");
    for delivery in legacy["state"]["deliveries"].as_array_mut().unwrap() {
        delivery[1].as_object_mut().unwrap().remove("hydrated_thread_roots");
    }
    std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let restored = SessionState::open(&store, 0, "identity").unwrap();
    assert_eq!(restored.sessions[&scope], "installed-conversation");
    assert!(restored.pending_loads.contains("installed-conversation"));
    assert!(restored.deliveries[&scope].standing_context_sent);
    assert!(restored.deliveries[&scope].delivered_event_ids.contains("retained-receipt"));
    assert!(restored.deliveries[&scope].hydrated_thread_roots.is_empty());
}
