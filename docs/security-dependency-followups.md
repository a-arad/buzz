# Dependency migration follow-ups

These open items track the exact advisory exceptions in `deny.toml` that
have no lock-only fix. The repository `a-arad/buzz` has issues disabled, so
this versioned tracker retains the work instead of silently dropping it.
Review every exception by **2026-10-23 UTC**; that date is a maintenance
commitment, not a cargo-deny-enforced expiry. DEP-001 matches the stable
0.2.x line. DEP-002 (standalone `nostr-relay-pool`) exists only on stable;
main's mesh-llm 0.76.2 uses nostr-sdk 0.45, which no longer pulls it in.

Reachability claims below come from `cargo tree --locked -e normal,build`
for every root workspace member on `x86_64-unknown-linux-gnu` and
`aarch64-apple-darwin`, plus the image build sets
(`-p buzz-relay -p buzz-admin -p buzz-pair-relay`, `-p buzz-push-gateway`,
`-p sprig`). They do not cover the separate `desktop/src-tauri` graph.

## DEP-001: Replace unsafe-libyaml

**Status: open.** Owner: codex-arad.
[RUSTSEC-2024-0449](https://rustsec.org/advisories/RUSTSEC-2024-0449.html)
reports an unmaintained parser with no patched release. The root dependency
path is `serde_yaml 0.9.34+deprecated → unsafe-libyaml 0.2.11`.

This parser is **production reachable**. In the relay,
`crates/buzz-relay/src/handlers/command_executor.rs::handle_workflow_def`
checks channel membership and then parses member-supplied YAML through
`crates/buzz-workflow/src/schema.rs::parse_yaml`. Membership does not make
that content trusted, and schema validation occurs after parsing. The
event-content limit in `handlers/ingest.rs` is not proof against parser
memory-safety or resource-exhaustion flaws. Persona/frontmatter, agent
hints, and Kubernetes configuration also use this dependency path.

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

## DEP-003: zip 9 via mesh-llm

**Status: open.** Owner: claude-arad.
[RUSTSEC-2026-0367](https://rustsec.org/advisories/RUSTSEC-2026-0367.html),
[0368](https://rustsec.org/advisories/RUSTSEC-2026-0368.html) and
[0369](https://rustsec.org/advisories/RUSTSEC-2026-0369.html) are
vulnerabilities in zip's extraction: privileged mode bits preserved, and
symlinks created or followed outside the destination. All three are patched
only in zip 9.0.0, which is semver-incompatible with the lock.

The only root path is `buzz-mesh-smoke [dev] → mesh-llm 0.76.2
(mesh-llm-host-runtime, -plugin-manager, -system) → zip ^8.6`. No root
workspace crate depends on zip, and zip is compiled into no shipped
artifact. The latest mesh-llm tag checked, v0.78.1, still locks zip 8.6.0.

At the pinned tag, mesh-llm extracts ZIP archives in
`mesh-llm-system/src/autoupdate/release_fetch.rs::extract_zip_archive` and
`mesh-llm-plugin-manager/src/archive.rs::extract_zip`. Both iterate entries
with `by_index`, reject paths without `enclosed_name`, and write regular
files with `File::create` and `io::copy`. Neither calls `extract()`, reads
`unix_mode`, or creates symlinks. That is why the exception is limited to
test builds; it is not a claim that zip 8.6 is safe for other callers.

Completion criteria:

- Adopt a mesh-llm release that uses zip ≥ 9 (or drops zip), run the mesh
  smoke and lifecycle tests, remove the three exact IDs, and pass
  `cargo-deny check`.
- Until then, at each mesh-llm pin change, re-run the reachability check
  above and re-read the mesh ZIP call sites; drop the exception if either
  claim stops holding.
- The desktop graph locks zip separately and is not covered here.

## DEP-004: Move mesh keyring off zbus 5

**Status: open.** Owner: claude-arad.
[RUSTSEC-2026-0370](https://rustsec.org/advisories/RUSTSEC-2026-0370.html)
(zvariant), [0371](https://rustsec.org/advisories/RUSTSEC-2026-0371.html)
(zbus_names) and [0372](https://rustsec.org/advisories/RUSTSEC-2026-0372.html)
(zvariant_derive) are unmaintained notices: the crates were merged into
zbus 6.0.0. There is no patched standalone version.

The only root path is `buzz-mesh-smoke [dev] → mesh-llm 0.76.2 →
keyring 4.2.0 → zbus-secret-service-keyring-store 1.0.1 → zbus ^5`. These
crates are compiled into no shipped artifact. zbus 6.0.0 was released on
2026-10-16, but the newest keyring (4.2.0) and keyring store (1.0.1) still
require zbus 5, and mesh-llm v0.78.1 still locks zbus 5.19.0.

Completion criteria:

- Adopt a keyring and mesh-llm release on zbus 6, run the mesh smoke and
  lifecycle tests, remove the three exact IDs, and pass `cargo-deny check`.
- Re-run the reachability check at each mesh-llm pin change; a normal-edge
  path would turn these into production maintenance risk and needs review.
