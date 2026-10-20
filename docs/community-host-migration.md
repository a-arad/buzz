# Moving an existing workspace to another hostname

`BUZZ_COMMUNITY_HOST_ALIASES` is a JSON object mapping additional request hosts
to existing community UUIDs. It defaults to `{}`. An alias resolves only an active
community; it never creates one. A canonical host row takes precedence, including
an archived row, so an alias cannot redirect another workspace's hostname.

The request keeps its actual hostname after resolving the community. NIP-42
WebSocket authentication and NIP-98 signed HTTP requests must still match that
hostname. Redirecting requests or rewriting signed URLs is not a migration.

1. Record the existing community ID and retain a database backup. Configure both
   the old and new hosts as aliases for that ID, preserving `RELAY_URL` initially.
2. Add DNS, TLS and ingress routes for the new hostname to the existing relay,
   including pairing. Preserve the old routes and their DNS. Add the new browser
   origin to the relay and proxy CORS lists.
3. Verify authenticated WebSocket and HTTP access through both hosts. Check the
   existing identity, channels, history, reminders, workflows and attachments.
4. Move clients at safe session boundaries. Preserve their identity and durable
   sessions; changing the relay address does not create a new workspace.
5. After both aliases are active, the existing `communities.host` may be renamed
   to the new host without changing `communities.id`. Then update `RELAY_URL`,
   pairing and media defaults. Updating `RELAY_URL` before the existing community
   has the new canonical host would cause startup to create another community.
6. Keep the old alias, DNS and certificate until every client has migrated.
   Existing signed media and message references may still need the old host.

Aliases are deployment configuration, so all relay replicas must receive the
same map. Rollback preserves the existing community row, database and client
identities; do not restore an older database to change a hostname.
