//! Restore the upstream ownership fence alongside durable provider sessions.
use super::*;

impl AgentPool {
    pub(super) fn restore_initial_ownership(&mut self) {
        let mut saved = Vec::new();
        let mut legacy_counts = HashMap::new();
        for agent in self
            .agents
            .iter()
            .flatten()
            .filter(|a| a.state.store.is_some())
        {
            for scope in agent.state.sessions.keys() {
                let generation = agent
                    .state
                    .scope_owner_generations
                    .get(scope)
                    .copied()
                    .unwrap_or(0);
                if generation == 0 {
                    *legacy_counts.entry(scope.clone()).or_insert(0usize) += 1;
                }
                saved.push((generation, agent.index, scope.clone()));
            }
        }
        self.next_scope_owner_generation = saved
            .iter()
            .map(|(g, _, _)| *g)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1);
        // Newer persisted fences win when an older worker had not yet returned
        // when the process stopped. Legacy checkpoints have no fence.
        saved.sort_by_key(|(generation, index, _)| (*generation, *index));
        for (generation, index, scope) in saved {
            if generation == 0 && legacy_counts.get(&scope).copied().unwrap_or(0) > 1 {
                // Preserve the legacy first-idle affinity until an actual
                // dispatch establishes the first authoritative fence. Choosing
                // a worker by slot number here would invent conversation history.
                continue;
            }
            let generation = if generation == 0 {
                self.record_scope_owner(scope.clone(), index)
            } else {
                self.session_owners.insert(
                    scope.clone(),
                    SessionOwner {
                        agent_index: index,
                        generation,
                    },
                );
                generation
            };
            if let Some(agent) = self.agents[index].as_mut() {
                agent.state.set_scope_owner_generation(scope, generation);
            }
        }
    }

    /// A respawn restores a checkpoint, unlike an ordinary task return. Rejoin
    /// its existing pool fence only if another worker has not taken ownership.
    pub(crate) fn return_restored_agent(&mut self, mut agent: OwnedAgent) {
        let scopes: Vec<_> = agent.state.sessions.keys().cloned().collect();
        for scope in scopes {
            let saved = agent
                .state
                .scope_owner_generations
                .get(&scope)
                .copied()
                .unwrap_or(0);
            let generation = match self.session_owners.get(&scope) {
                Some(owner)
                    if owner.agent_index == agent.index
                        && (saved == 0 || saved == owner.generation) =>
                {
                    Some(owner.generation)
                }
                Some(_) => None,
                None if saved == 0 => None,
                None => Some(self.record_scope_owner(scope.clone(), agent.index)),
            };
            if let Some(generation) = generation {
                agent.state.set_scope_owner_generation(scope, generation);
            }
        }
        self.return_agent(agent);
    }
}
