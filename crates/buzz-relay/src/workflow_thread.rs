//! Validate the explicit destination at authoring and again before delivery.
use crate::state::AppState;
use buzz_core::tenant::CommunityId;
use chrono::{DateTime, Utc};
use uuid::Uuid;

pub(crate) struct Root {
    pub id: Vec<u8>,
    pub created_at: DateTime<Utc>,
}

pub(crate) async fn resolve(
    community: CommunityId,
    id: &str,
    channel: Uuid,
    state: &AppState,
) -> Result<Root, String> {
    let bytes = hex::decode(id).map_err(|_| "invalid thread root hex".to_string())?;
    let event = state
        .db
        .get_event_by_id(community, &bytes)
        .await
        .map_err(|e| format!("root lookup: {e}"))?
        .ok_or_else(|| "thread root not found".to_string())?;
    if event.channel_id != Some(channel) {
        return Err("thread root belongs to a different channel".into());
    }
    if !matches!(event.event.kind.as_u16(), 9 | 40002 | 45001 | 45003) {
        return Err("thread root must be a message".into());
    }
    let meta = state
        .db
        .get_thread_metadata_by_event(community, &bytes)
        .await
        .map_err(|e| format!("thread metadata lookup: {e}"))?;
    let is_reply = event.event.tags.iter().any(|tag| {
        let parts = tag.as_slice();
        parts.len() >= 4
            && parts[0] == "e"
            && (parts[3] == "reply" || (parts[3] == "root" && parts[1] != id))
    }) || meta.is_some_and(|m| {
        m.parent_event_id.is_some() || m.root_event_id.is_some_and(|r| r != bytes)
    });
    if is_reply {
        return Err("thread_root must identify the canonical root".into());
    }
    let created_at = DateTime::from_timestamp(event.event.created_at.as_secs() as i64, 0)
        .ok_or_else(|| "invalid root timestamp".to_string())?;
    Ok(Root {
        id: bytes,
        created_at,
    })
}
