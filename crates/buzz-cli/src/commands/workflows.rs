use sha2::{Digest, Sha256};

use crate::client::{
    extract_d_tag, extract_relay_response_field, normalize_write_response, print_create_response,
    BuzzClient,
};
use crate::error::CliError;
use crate::validate::{parse_uuid, read_or_stdin, sdk_err, validate_uuid};

// TODO(phase-4): Replace raw nostr::EventBuilder usage with buzz-sdk builder functions

/// List workflows in a channel — query kind:30620 workflow definition events.
pub async fn cmd_list_workflows(
    client: &BuzzClient,
    channel_id: Option<&str>,
    owned: bool,
) -> Result<(), CliError> {
    let mut filter = serde_json::json!({
        "kinds": [30620],
    });
    if let Some(channel_id) = channel_id {
        validate_uuid(channel_id)?;
        filter["#h"] = serde_json::json!([channel_id]);
    }
    if owned {
        filter["authors"] = serde_json::json!([client.keys().public_key().to_hex()]);
    }
    let events = client.query_all(filter).await?;
    let workflows: Vec<serde_json::Value> = events
        .iter()
        .map(|e| {
            serde_json::json!({
                "workflow_id": extract_d_tag(e),
                "event_id": e.get("id"),
                "channel_id": crate::client::extract_tag_value(e, "h"),
                "content": e.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                "created_at": e.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0),
                "pubkey": e.get("pubkey").and_then(|v| v.as_str()).unwrap_or(""),
            })
        })
        .collect();
    let output = serde_json::to_string(&workflows).unwrap_or_default();
    println!("{output}");
    Ok(())
}

/// Get a single workflow definition.
pub async fn cmd_get_workflow(client: &BuzzClient, workflow_id: &str) -> Result<(), CliError> {
    validate_uuid(workflow_id)?;
    let filter = serde_json::json!({
        "kinds": [30620],
        "#d": [workflow_id]
    });
    let resp = client.query(&filter).await?;
    let events: Vec<serde_json::Value> = serde_json::from_str(&resp).unwrap_or_default();
    if let Some(e) = events.first() {
        let normalized = serde_json::json!({
            "workflow_id": extract_d_tag(e),
            "event_id": e.get("id"),
            "channel_id": crate::client::extract_tag_value(e, "h"),
            "content": e.get("content").and_then(|v| v.as_str()).unwrap_or(""),
            "created_at": e.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0),
            "pubkey": e.get("pubkey").and_then(|v| v.as_str()).unwrap_or(""),
        });
        println!("{normalized}");
    } else {
        println!("null");
    }
    Ok(())
}

/// Get workflow run history — query kinds [46001, 46002, 46003].
///
/// NOTE: The relay does not currently emit workflow execution events (46001-46003).
/// Run history is stored in the workflow_runs DB table, not as Nostr events.
/// This command will return an empty array until the relay adds event emission
/// or a dedicated REST endpoint for run history.
pub async fn cmd_get_workflow_runs(
    client: &BuzzClient,
    workflow_id: &str,
    limit: Option<u32>,
) -> Result<(), CliError> {
    validate_uuid(workflow_id)?;
    let limit = limit.unwrap_or(20).min(100);
    let filter = serde_json::json!({
        "kinds": [46001, 46002, 46003],
        "#d": [workflow_id],
        "limit": limit
    });
    let resp = client.query(&filter).await?;
    let events: Vec<serde_json::Value> = serde_json::from_str(&resp).unwrap_or_default();
    let normalized: Vec<serde_json::Value> = events
        .iter()
        .map(|e| {
            serde_json::json!({
                "event_id": e.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "kind": e.get("kind").and_then(|v| v.as_u64()).unwrap_or(0),
                "content": e.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                "created_at": e.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0),
                "tags": e.get("tags").cloned().unwrap_or(serde_json::json!([])),
            })
        })
        .collect();
    let output = serde_json::to_string(&normalized).unwrap_or_default();
    println!("{output}");
    Ok(())
}

/// Create a workflow — sign and submit a kind:30620 event.
pub async fn cmd_create_workflow(
    client: &BuzzClient,
    channel_id: &str,
    yaml: &str,
) -> Result<(), CliError> {
    let channel_uuid = parse_uuid(channel_id)?;
    let yaml_definition = read_or_stdin(yaml)?;

    let workflow_id = uuid::Uuid::new_v4();
    let builder = buzz_sdk::build_workflow_def(channel_uuid, workflow_id, &yaml_definition)
        .map_err(sdk_err)?;
    let event = client.sign_event(builder)?;

    let resp = client.submit_event(event).await?;
    let final_workflow_id = extract_relay_response_field(&resp, "workflow_id")
        .unwrap_or_else(|| workflow_id.to_string());
    print_create_response(&resp, "workflow_id", &final_workflow_id);
    Ok(())
}

/// Update a workflow — sign and submit an updated kind:30620 event with same d-tag.
pub async fn cmd_update_workflow(
    client: &BuzzClient,
    channel_id: &str,
    workflow_id: &str,
    yaml: &str,
    if_event: Option<&str>,
) -> Result<(), CliError> {
    let channel_uuid = parse_uuid(channel_id)?;
    let wf_uuid = parse_uuid(workflow_id)?;
    let yaml_definition = read_or_stdin(yaml)?;

    let filter = serde_json::json!({
        "kinds": [30620],
        "#d": [workflow_id]
    });
    let resp = client.query(&filter).await?;
    let events: Vec<serde_json::Value> = serde_json::from_str(&resp).unwrap_or_default();
    let expected_revision = events
        .first()
        .and_then(|event| event.get("id"))
        .and_then(|id| id.as_str())
        .ok_or_else(|| CliError::NotFound(format!("workflow {workflow_id} not found")))?;
    if let Some(reviewed) = if_event {
        crate::validate::validate_hex64(reviewed)?;
        if reviewed != expected_revision {
            return Err(CliError::Conflict("workflow changed since review".into()));
        }
    }

    let builder =
        buzz_sdk::build_workflow_update(channel_uuid, wf_uuid, &yaml_definition, expected_revision)
            .map_err(sdk_err)?;
    // NIP-33 replacements order by timestamp before event ID. A reviewed
    // update in the same second must supersede its predecessor deterministically.
    let previous_at = events
        .first()
        .and_then(|event| event["created_at"].as_u64())
        .ok_or_else(|| CliError::Other("workflow head has no creation time".into()))?;
    let builder = builder.custom_created_at(nostr::Timestamp::from_secs(
        nostr::Timestamp::now()
            .as_secs()
            .max(previous_at.saturating_add(1)),
    ));
    let event = client.sign_event(builder)?;

    let resp = client.submit_event(event).await?;
    println!("{}", normalize_write_response(&resp));
    Ok(())
}

/// Delete a workflow — sign and submit a kind:5 deletion event.
pub async fn cmd_delete_workflow(client: &BuzzClient, workflow_id: &str) -> Result<(), CliError> {
    let wf_uuid = parse_uuid(workflow_id)?;
    let keys = client.keys();

    let builder =
        buzz_sdk::build_workflow_delete(&keys.public_key().to_hex(), wf_uuid).map_err(sdk_err)?;
    let event = client.sign_event(builder)?;

    let resp = client.submit_event(event).await?;
    println!("{}", normalize_write_response(&resp));
    Ok(())
}

/// Trigger a workflow — sign and submit a kind:46020 event.
///
/// When `inputs` is provided, it is parsed as a JSON object and used as the
/// event content (MCP parity). When omitted, the event content is `{}`.
pub async fn cmd_trigger_workflow(
    client: &BuzzClient,
    workflow_id: &str,
    inputs: Option<&str>,
) -> Result<(), CliError> {
    let wf_uuid = parse_uuid(workflow_id)?;

    if let Some(raw) = inputs {
        // Parse and validate it is a JSON object, then build the event manually
        // so we can embed the inputs as the event content.
        let parsed: serde_json::Value = serde_json::from_str(raw)
            .map_err(|e| CliError::Usage(format!("--inputs is not valid JSON: {e}")))?;
        if !parsed.is_object() {
            return Err(CliError::Usage("--inputs must be a JSON object".into()));
        }
        let content = serde_json::to_string(&parsed).unwrap_or_default();
        use nostr::{EventBuilder, Kind, Tag};
        let tags = vec![Tag::parse(["d", &wf_uuid.to_string()])
            .map_err(|e| CliError::Other(format!("tag error: {e}")))?];
        let builder = EventBuilder::new(
            Kind::Custom(buzz_sdk::kind::KIND_WORKFLOW_TRIGGER as u16),
            &content,
        )
        .tags(tags);
        let event = client.sign_event(builder)?;
        let resp = client.submit_event(event).await?;
        println!("{}", normalize_write_response(&resp));
    } else {
        let builder = buzz_sdk::build_workflow_trigger(wf_uuid).map_err(sdk_err)?;
        let event = client.sign_event(builder)?;
        let resp = client.submit_event(event).await?;
        println!("{}", normalize_write_response(&resp));
    }
    Ok(())
}

/// Approve or deny a workflow step — sign and submit a kind:46030 (grant) or 46031 (deny) event.
pub async fn cmd_approve_step(
    client: &BuzzClient,
    approval_token: &str,
    approved: bool,
    note: Option<&str>,
) -> Result<(), CliError> {
    validate_uuid(approval_token)?;

    let content = note.unwrap_or("");

    // The relay expects d-tag = hex(SHA256(token)), not the raw token UUID.
    let token_hash = hex::encode(Sha256::digest(approval_token.as_bytes()));
    let builder =
        buzz_sdk::build_workflow_approval(&token_hash, approved, content).map_err(sdk_err)?;
    let event = client.sign_event(builder)?;

    let resp = client.submit_event(event).await?;
    println!("{}", normalize_write_response(&resp));
    Ok(())
}

pub async fn dispatch(cmd: crate::WorkflowsCmd, client: &BuzzClient) -> Result<(), CliError> {
    use crate::WorkflowsCmd;
    match cmd {
        WorkflowsCmd::List { channel, owned } => {
            cmd_list_workflows(client, channel.as_deref(), owned).await
        }
        WorkflowsCmd::Get { workflow } => cmd_get_workflow(client, &workflow).await,
        WorkflowsCmd::Create { channel, yaml } => {
            cmd_create_workflow(client, &channel, &yaml).await
        }
        WorkflowsCmd::Update {
            channel,
            workflow,
            yaml,
            if_event,
        } => cmd_update_workflow(client, &channel, &workflow, &yaml, if_event.as_deref()).await,
        WorkflowsCmd::Delete { workflow } => cmd_delete_workflow(client, &workflow).await,
        WorkflowsCmd::Trigger { workflow, inputs } => {
            cmd_trigger_workflow(client, &workflow, inputs.as_deref()).await
        }
        WorkflowsCmd::Runs { workflow, limit } => {
            cmd_get_workflow_runs(client, &workflow, limit).await
        }
        WorkflowsCmd::Approve {
            token,
            approved,
            note,
        } => {
            // approved is already a bool — no parse_bool_flag needed
            cmd_approve_step(client, &token, approved, note.as_deref()).await
        }
    }
}
