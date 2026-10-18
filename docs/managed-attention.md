# Managed attention and scheduled thread destinations

A scheduled `send_message` action can name an existing canonical `thread_root`:

```yaml
name: Review the release
trigger:
  on: schedule
  cron: '0 0 9 20 10 * 2026'
steps:
  - id: wake
    action: send_message
    thread_root: <64-character lowercase root event ID>
    text: '@commissioned-agent Reassess the retained release evidence'
```

`thread_root` is literal, not template-expanded, and cannot combine with
`reply_in_thread`. Authoring validates that the root exists, is a message,
belongs to the destination channel, and is canonical. The owner needs membership
in the destination, including when the step overrides the workflow channel.
Execution resolves the same ancestry through the existing reply sink; a missing
parent fails the action rather than producing a new top-level message. Direct
replies use NIP-10's single `reply` marker for both root and parent. The native
thread session policy resolves that marker to the original task.

Relays advertise `buzz-workflow-thread-root-v1` in NIP-11 `supported_extensions`.
Older relays can silently ignore unknown YAML fields; managed authors must check
this capability before relying on routing. `workflows list --owned` paginates the
current identity's accessible definitions, including legacy schedules. List/get
include `event_id` and `channel_id`; `workflows update --if-event` rejects a stale
review. Updates advance the replaceable event timestamp so same-second edits
and cancellation deterministically replace the previous head.

When `BUZZ_ATTENTION_COMMAND` names an installed inventory command, `buzz-acp`
executes `<command> list --context <originating-link>` before each new prompt.
The response must be JSON with `complete: true`; errors, nonzero exits, over
64 KiB of stdout, or a 150-second timeout produce an explicit unavailable
inventory, not an empty set. No command is run unless commissioned.

The prompt includes the inventory and `_meta.axesAttentionContext` records the
origin link, channel/thread policy, and (for a private reminder) trigger ID and
revision. The exact blocks and metadata are checkpointed before native dispatch,
so recovery reuses the same accepted request. Reusing an ACP worker clears the
previous turn's context. Each private reminder occurrence still gets its own
native conversation in its commissioned workspace; retries keep that occurrence.
A legacy reminder with no origin reads the owner's unscoped inventory and states
that an origin must be selected before authoring.

The companion Edamame command enforces owned-set review at authoring and
notification disposition. Its installed command prefix retires ordinary direct
reminder/workflow authoring. This opt-in harness integration alone does not
remove raw credential authority or force a model to complete its work. Delivery
receipts remain distinct from reminder completion. Controller expiry remains
independent of the model and of this inventory hook.

Run `python3 scripts/test-workflow-thread-root.py --buzz <built-buzz> --relay
http://127.0.0.1:<isolated-port>` against a disposable local relay. The script
creates only fresh test identities and channels, waits for an actual scheduled
reply, checks its destination and owner, rejects invalid roots and stale writes,
and disables its workflow. It refuses non-loopback hosts. It does not start a
relay or clear a database. ACP unit tests separately exercise prompt preparation,
fresh reminder occurrences and retained recovery requests.
