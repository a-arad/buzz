# Publishing an already-tested relay image

Use the fork's `Publish tested relay artifact` workflow to publish a validated
relay image to `ghcr.io/<owner>/<repository>` without rebuilding it. Normal
upstream relay builds retain their existing Docker workflow.

1. Publish the exact source commit through the repository's configured Git
   access. Use a distinct `axes-relay-...` tag for a custom release.
2. Attach the tested OCI image archive to that GitHub release as
   `relay-image.tar`. Record its SHA-256, image manifest digest, platform,
   source commit, and validation results in the release notes or an attached
   receipt. Include the archive's OCI index and blobs so the manifest can be
   preserved exactly.
3. Dispatch `publish-tested-relay.yml` from accepted fork main with the release
   tag, verified archive SHA-256, and verified manifest digest. It uses the
   repository's short-lived GitHub Actions token, verifies the archive and
   source manifest, and requires the registry copy to preserve the digest.
4. For the first publication, make the container package public in GitHub
   package settings, matching the public source and release artifact. Verify
   anonymous retrieval of the exact digest before changing production's image
   reference. Subsequent publications use the existing package.
5. Deploy by digest and verify the running artifact. Keep the preceding
   deployment reference and recovery image available for rollback.

Publish only the image and release evidence suitable for the public fork.
Deployment credentials, Helm values, database backups, and private operational
records remain in the existing private release store.

The initial agent-DM release source is
`cc8ce0ee6bd9502e287fb7e93b039741ae2214dc` on
`codex/agent-dm-policy-relay-v0.2.1`. The retained Git bundle remains a recovery
copy; ordinary Git access can now retrieve this branch.
