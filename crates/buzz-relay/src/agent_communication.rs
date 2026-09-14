//! Optional deployment policy for agent-to-agent direct messages.
//!
//! Only explicitly configured human public keys are exempt. Editable profiles,
//! ownership attestations and ephemeral gift-wrap authors cannot grant exemption.

use std::collections::HashSet;

use buzz_core::kind::*;
use buzz_core::tenant::TenantContext;
use nostr::{Event, PublicKey};

use crate::config::ConfigError;
use crate::handlers::ingest::{IngestAuth, IngestError};
use crate::state::AppState;

/// Deployment policy limiting every DM to at most one non-human identity.
#[derive(Debug, Clone)]
pub struct AgentCommunicationPolicy {
    human_pubkeys: HashSet<PublicKey>,
}

impl AgentCommunicationPolicy {
    /// An absent setting disables the policy; empty or malformed settings fail startup.
    pub fn parse(humans: Option<&str>) -> Result<Option<Self>, ConfigError> {
        let Some(humans) = humans else {
            return Ok(None);
        };
        let human_pubkeys = humans
            .split(',')
            .map(|value| {
                PublicKey::from_hex(value.trim()).map_err(|_| {
                    ConfigError::InvalidValue(
                        "BUZZ_DM_HUMAN_PUBKEYS requires nonempty 64-hex public keys".into(),
                    )
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Some(Self { human_pubkeys }))
    }

    /// Read the operator-owned allowlist, rejecting invalid environment encoding.
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        match std::env::var("BUZZ_DM_HUMAN_PUBKEYS") {
            Ok(value) => Self::parse(Some(&value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(_) => Err(ConfigError::InvalidValue(
                "BUZZ_DM_HUMAN_PUBKEYS must be valid Unicode".into(),
            )),
        }
    }

    fn is_human(&self, key: &PublicKey) -> bool {
        self.human_pubkeys.contains(key)
    }

    fn check_dm(
        &self,
        participants: impl IntoIterator<Item = PublicKey>,
    ) -> Result<(), IngestError> {
        let agents: HashSet<_> = participants
            .into_iter()
            .filter(|key| !self.is_human(key))
            .collect();
        if agents.len() > 1 {
            return Err(deny(
                "agent-to-agent DMs are disabled; use a shared channel",
            ));
        }
        Ok(())
    }

    /// Validate the already-decoded participant set before persisting a DM command.
    pub(crate) fn check_participants(&self, keys: &[Vec<u8>]) -> Result<(), IngestError> {
        let keys = keys
            .iter()
            .map(|key| {
                PublicKey::from_slice(key).map_err(|_| {
                    IngestError::Internal("error: invalid stored DM participant key".into())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.check_dm(keys)
    }

    fn check_gift_wrap(
        &self,
        actor: PublicKey,
        recipients: &[PublicKey],
    ) -> Result<(), IngestError> {
        // The connection identity is authoritative; NIP-59 outer authors are ephemeral.
        if self.is_human(&actor) {
            return Ok(());
        }
        if recipients.len() != 1 {
            return Err(deny("agent encrypted DMs require exactly one recipient"));
        }
        // Self copies retain the sender's human-DM history without delivery to another agent.
        self.check_dm([actor, recipients[0]])
    }
}

fn deny(message: &str) -> IngestError {
    IngestError::Rejected(format!("restricted: {message}"))
}

/// Enforce channel-less transport restrictions after signature and authentication checks.
pub(crate) fn validate_event(
    state: &AppState,
    event: &Event,
    auth: &IngestAuth,
) -> Result<(), IngestError> {
    let Some(policy) = state.config.agent_communication.as_ref() else {
        return Ok(());
    };
    let kind = event.kind.as_u16() as u32;
    if kind == KIND_GIFT_WRAP && !policy.is_human(auth.pubkey()) {
        let recipients = event
            .tags
            .iter()
            .filter(|tag| tag.as_slice().first().is_some_and(|v| v == "p"))
            .map(|tag| {
                tag.content()
                    .and_then(|value| PublicKey::from_hex(value).ok())
                    .ok_or_else(|| deny("invalid encrypted DM recipient"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        return policy.check_gift_wrap(*auth.pubkey(), &recipients);
    }
    if kind == KIND_NIP29_CREATE_GROUP
        && event.tags.iter().any(|tag| {
            let parts = tag.as_slice();
            parts.len() >= 2 && parts[0] == "channel_type" && parts[1] == "dm"
        })
    {
        return Err(deny("create DMs through the DM command"));
    }
    Ok(())
}

/// Check the resolved channel, including events whose channel is derived from a target.
pub(crate) async fn validate_channel(
    tenant: &TenantContext,
    state: &AppState,
    event: &Event,
    channel: &buzz_db::channel::ChannelRecord,
    auth: &IngestAuth,
) -> Result<(), IngestError> {
    let Some(policy) = state.config.agent_communication.as_ref() else {
        return Ok(());
    };
    if channel.channel_type != "dm" {
        return Ok(());
    }
    // DM command expansion creates a new immutable set. Generic membership writes
    // must not create an unchecked set, including concurrent additions or public joins.
    let kind = event.kind.as_u16() as u32;
    if matches!(kind, KIND_NIP29_PUT_USER | KIND_NIP29_JOIN_REQUEST) {
        return Err(deny(
            "DM participants are immutable; use the DM add-member command",
        ));
    }
    let mut keys = state
        .db
        .get_members(tenant.community(), channel.id)
        .await
        .map_err(|error| IngestError::Internal(format!("error: DM membership lookup: {error}")))?
        .into_iter()
        .map(|member| member.pubkey)
        .collect::<Vec<_>>();
    keys.push(auth.pubkey().to_bytes().to_vec());
    policy.check_participants(&keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::Keys;

    fn fixture() -> (
        AgentCommunicationPolicy,
        PublicKey,
        PublicKey,
        PublicKey,
        PublicKey,
    ) {
        let h1 = Keys::generate().public_key();
        let h2 = Keys::generate().public_key();
        let a1 = Keys::generate().public_key();
        let a2 = Keys::generate().public_key();
        let p = AgentCommunicationPolicy::parse(Some(&format!("{},{}", h1.to_hex(), h2.to_hex())))
            .unwrap()
            .unwrap();
        (p, h1, h2, a1, a2)
    }

    #[test]
    fn dm_policy_preserves_human_pairs_and_deduplicates_self() {
        let (p, h1, h2, a1, _) = fixture();
        for keys in [
            vec![h1, h2],
            vec![h1, a1],
            vec![a1, h1],
            vec![h1, h2, a1],
            vec![a1, a1, h1],
        ] {
            assert!(p.check_dm(keys).is_ok());
        }
    }

    #[test]
    fn dm_policy_rejects_two_agents_even_with_a_human_present() {
        let (p, h1, _, a1, a2) = fixture();
        assert!(p.check_dm([a1, a2]).is_err());
        assert!(p.check_dm([h1, a1, a2]).is_err());
    }

    #[test]
    fn encrypted_dm_uses_authenticated_actor_and_preserves_human_and_self_delivery() {
        let (p, h1, _, a1, a2) = fixture();
        assert!(p.check_gift_wrap(a1, &[a2]).is_err());
        assert!(p.check_gift_wrap(a1, &[h1]).is_ok());
        assert!(p.check_gift_wrap(a1, &[a1]).is_ok());
        assert!(p.check_gift_wrap(h1, &[a1]).is_ok());
        assert!(p.check_gift_wrap(a1, &[]).is_err());
        assert!(p.check_gift_wrap(a1, &[h1, a2]).is_err());
    }

    #[test]
    fn absent_policy_is_disabled_and_malformed_policy_fails_closed() {
        assert!(AgentCommunicationPolicy::parse(None).unwrap().is_none());
        for humans in ["", "bad", ",", " "] {
            assert!(AgentCommunicationPolicy::parse(Some(humans)).is_err());
        }
    }
}
