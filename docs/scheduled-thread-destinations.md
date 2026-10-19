# Scheduled task continuations on the stable relay

Scheduled `send_message` steps may supply a literal `thread_root` containing the
canonical root event's lowercase 64-character ID. The relay checks the root's
message kind, channel, ancestry and the owner's destination membership during
authoring, and rechecks the destination before sending. Missing or invalid roots
fail explicitly. The action persists a direct reply with thread metadata and
emits the existing live thread summary so the UI and thread-scoped agent agree.

The relay advertises `buzz-workflow-thread-root-v1` in NIP-11. Clients must check
that capability: older relays silently ignore unknown YAML fields. This stable
backport accepts explicit roots and rejects `reply_in_thread: true`; implicit
trigger replies remain a newer-line feature. Recurring expiry and owned-set
review belong to the companion managed attention interface, not this routing
field.

The companion CLI provides `workflows list --owned`, revision IDs in inventory,
and `workflows update --if-event`. Its timestamp handling makes same-second
updates and cancellation supersede the predecessor deterministically.

With that CLI and a disposable local relay, run:

```
python3 scripts/test-workflow-thread-root.py --buzz /path/to/buzz --relay http://127.0.0.1:30484
```

The command creates fresh test identities/channels, waits for an actual scheduled
reply, verifies its owning thread and mention, rejects invalid destinations and
stale writes, and disables the test workflow. It refuses non-loopback hosts and
does not start a relay or erase a database.
