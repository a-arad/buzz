# Dependency migration follow-ups

These open items track the exact advisory exceptions in `deny.toml`.
DEP-001 and DEP-002 are unmaintained notices. DEP-003 covers vulnerability
advisories, limited to test builds; see its reachability evidence. The repository
`a-arad/buzz` has issues disabled, so this versioned tracker and
[PR #15](https://github.com/a-arad/buzz/pull/15) retain the work instead of
silently dropping it. Owner: codex-arad under Arad's maintenance delegation.
Review both exceptions by **2026-10-23 UTC**; that date is a maintenance
commitment, not a cargo-deny-enforced expiry.

## DEP-001: Replace unsafe-libyaml

**Status: open.** [RUSTSEC-2024-0449](https://rustsec.org/advisories/RUSTSEC-2024-0449.html)
reports an unmaintained parser with no patched release. The root dependency
path is `serde_yaml 0.9.34+deprecated → unsafe-libyaml 0.2.11`.

This parser is **production reachable**. In the relay,
`crates/buzz-relay/src/handlers/command_executor.rs::handle_workflow_def`
checks channel membership and then parses member-supplied YAML through
`crates/buzz-workflow/src/schema.rs::parse_yaml`. Membership does not make
that content trusted, and schema validation occurs after parsing. The
256 KiB event-content limit in `handlers/ingest.rs` is not proof against
parser memory-safety or resource-exhaustion flaws. Persona/frontmatter,
agent hints, and Kubernetes configuration also use this dependency path.

The exception accepts the maintenance risk temporarily; it does not claim
the parser is safe or unreachable. A lock-only update cannot remove it.
Switching to `serde_yaml_ng` is insufficient: the advisory explicitly lists
that fork as another user of unsafe-libyaml. Evaluate a maintained safe-Rust
parser, such as `serde-saphyr`, against the actual serde API and YAML behavior
before replacement; no compatibility claim is made here.

Completion criteria:

- Migrate direct YAML consumers and the transitive kube-client path, or
  explicitly track any remaining dependency before claiming removal.
- Verify stored workflow compatibility and canonical JSON, duplicate-key and
  tag handling, aliases, malformed/deep input, and bounded resource use;
  preserve authorization before parsing.
- Run full affected packages, workflow PostgreSQL integration, persona/hint
  fixtures, and the Kubernetes provider's configuration tests.
- Confirm `cargo tree --locked --target all -i unsafe-libyaml` has no root
  workspace path; check the separate desktop graph too. Remove this exact
  exception and pass `cargo-deny check` before closing DEP-001.

## DEP-002: Migrate mesh Nostr and desktop lock

**Status: open.** [RUSTSEC-2026-0243](https://rustsec.org/advisories/RUSTSEC-2026-0243.html)
reports that standalone `nostr-relay-pool` is unmaintained; its functionality
moved into `nostr-sdk` starting at 0.45.0. There is no patched standalone
pool version. The root relay path is `buzz-relay [dev] → mesh-llm 0.74.0 →
nostr-sdk 0.44.1 → nostr-relay-pool 0.44.3`. Inspection with normal/build
edges finds no pool path in the root relay production graph; it is used by
mesh tests/examples. This claim does not cover the separate desktop graph,
whose optional mesh integration may enable the SDK at runtime.

Completion criteria:

- Qualify a mesh-llm update that migrates to the integrated SDK, preserving
  discovery, relay authentication, events, reconnects and subscriptions.
  A broad mesh or SDK migration is outside the narrow relay repair.
- Run the full relay package and mesh lifecycle/integration tests; keep the
  already recorded intermittent mesh timeout separate from new failures.
- Confirm pool removal in both root and desktop dependency graphs, remove
  the exact exception, and pass dependency policy before closing DEP-002.

## DEP-003: zip 9 via mesh-llm

**Status: open.** Owner: claude-arad.
[RUSTSEC-2026-0367](https://rustsec.org/advisories/RUSTSEC-2026-0367.html),
[0368](https://rustsec.org/advisories/RUSTSEC-2026-0368.html) and
[0369](https://rustsec.org/advisories/RUSTSEC-2026-0369.html) are
vulnerabilities in zip's extraction: privileged mode bits preserved, and
symlinks created or followed outside the destination. All three are patched
only in zip 9.0.0, which is semver-incompatible with the lock.

The only root path is `buzz-relay [dev] → mesh-llm 0.74.0
(mesh-llm-host-runtime, -plugin-manager, -system) → zip ^2`, locked at
2.4.2. No root workspace crate depends on zip. `cargo tree --locked
-e normal,build` for every root member on `x86_64-unknown-linux-gnu` and
`aarch64-apple-darwin`, and for the image set
`-p buzz-relay -p buzz-admin -p buzz-pair-relay`, finds no zip path, so zip
is compiled into no shipped artifact. The latest mesh-llm tag checked,
v0.78.1, still locks zip 8.6.0.

At the pinned tag, mesh-llm extracts ZIP archives in
`mesh-llm-system/src/autoupdate/release_fetch.rs::extract_zip_archive` and
`mesh-llm-plugin-manager/src/archive.rs::extract_zip`. Both iterate entries
with `by_index`, reject paths without `enclosed_name`, and write regular
files with `File::create` and `io::copy`. Neither calls `extract()`, reads
`unix_mode`, or creates symlinks. That is why the exception is limited to
test builds; it is not a claim that zip 2.4 is safe for other callers.

Completion criteria:

- Adopt a mesh-llm release that uses zip ≥ 9 (or drops zip), run the full
  relay package and mesh lifecycle tests, remove the three exact IDs, and
  pass `cargo-deny check`.
- Until then, at each mesh-llm pin change, re-run the reachability check
  above and re-read the mesh ZIP call sites; drop the exception if either
  claim stops holding.
- The desktop graph locks zip separately and is not covered here.

**Separate desktop security follow-up, still open:**
`desktop/src-tauri/Cargo.lock` remains at rustls 0.23.42 and webbrowser 1.2.1.
It is not the root lock used by this relay artifact. Update it separately to
at least rustls 0.23.46 and webbrowser 1.2.2, qualify native desktop builds
and the browser-auth flow, and audit that graph explicitly. The root-lock
repair does not establish that desktop is patched. References:
[rustls 0.23.46 release](https://github.com/rustls/rustls/releases/tag/v/0.23.46),
[webbrowser advisory](https://rustsec.org/advisories/RUSTSEC-2026-0257.html).
