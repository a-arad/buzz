#!/usr/bin/env python3
"""Exercise scheduled thread routing with the real CLI and an isolated local relay."""
import argparse
from datetime import datetime, timedelta, timezone
import json
import os
import secrets
import subprocess
import time
from urllib.parse import urlparse
from urllib.request import Request, urlopen


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--buzz', required=True)
    parser.add_argument('--relay', required=True)
    args = parser.parse_args()
    if urlparse(args.relay).hostname not in {'localhost', '127.0.0.1', '::1'}:
        raise SystemExit('Use an isolated loopback relay; this test creates disposable state.')
    env = {k: v for k, v in os.environ.items() if not k.startswith('BUZZ_')}
    env.update(BUZZ_RELAY_URL=args.relay, BUZZ_PRIVATE_KEY=secrets.token_hex(32))

    def call(*words, content=None, rejected=False):
        run = subprocess.run([args.buzz, *words], env=env, input=content,
                             text=True, capture_output=True, timeout=30)
        value = json.loads(run.stdout) if run.stdout.strip() else None
        if rejected:
            assert run.returncode != 0 or (isinstance(value, dict) and value.get('accepted') is False), words
            return value
        assert run.returncode == 0, (words, run.stderr)
        assert not isinstance(value, dict) or value.get('accepted', True), value
        return value

    with urlopen(Request(args.relay, headers={'Accept': 'application/nostr+json'}), timeout=5) as response:
        assert 'buzz-workflow-thread-root-v1' in json.load(response)['supported_extensions']
    label = 'thread-smoke-' + secrets.token_hex(5)
    call('users', 'set-profile', '--name', label)
    owner = call('users', 'get')[0]['pubkey']
    first = call('channels', 'create', '--name', label, '--type', 'stream', '--visibility', 'open')
    channel = first['channel_id']
    other = call('channels', 'create', '--name', label + '-other', '--type', 'stream', '--visibility', 'open')['channel_id']
    root = call('messages', 'send', '--channel', channel, '--content', 'Owning task')['event_id']
    child = call('messages', 'send', '--channel', channel, '--reply-to', root, '--content', 'Task continuation')['event_id']
    other_root = call('messages', 'send', '--channel', other, '--content', 'Different task')['event_id']
    at = datetime.now(timezone.utc) + timedelta(seconds=12)
    definition = {'name': label, 'enabled': True, 'trigger': {'on': 'schedule',
        'cron': f'{at.second} {at.minute} {at.hour} {at.day} {at.month} * {at.year}'},
        'steps': [{'id': 'wake', 'action': 'send_message', 'thread_root': root,
                   'text': '@' + label + ' Scheduled continuation'}]}
    for invalid in ['0' * 64, child, other_root, first['event_id']]:
        bad = dict(definition, steps=[dict(definition['steps'][0], thread_root=invalid)])
        call('workflows', 'create', '--channel', channel, '--yaml', '-', content=json.dumps(bad), rejected=True)
    created = call('workflows', 'create', '--channel', channel, '--yaml', '-', content=json.dumps(definition))
    workflow = created['workflow_id']
    head = call('workflows', 'get', '--workflow', workflow)
    assert head['channel_id'] == channel and head['event_id'] == created['event_id']
    assert any(w['workflow_id'] == workflow for w in call('workflows', 'list', '--owned'))
    other_env = dict(env, BUZZ_PRIVATE_KEY=secrets.token_hex(32))
    foreign = subprocess.run([args.buzz, 'workflows', 'list', '--owned'], env=other_env,
                             text=True, capture_output=True, timeout=30, check=True)
    assert json.loads(foreign.stdout) == []
    deadline = time.monotonic() + 85
    wake = None
    while time.monotonic() < deadline:
        messages = call('messages', 'thread', '--channel', channel, '--event', root)
        wake = next((m for m in messages if 'Scheduled continuation' in m.get('content', '')), None)
        if wake:
            break
        time.sleep(1)
    assert wake is not None, 'Scheduled action did not execute in its owning thread'
    # NIP-10's single reply marker represents both root and parent for a
    # direct reply. The native harness uses that same canonical interpretation.
    roots = [tag[1] for tag in wake['tags'] if len(tag) > 3 and tag[0] == 'e' and tag[3] == 'root']
    assert not roots or roots == [root], wake['tags']
    assert ['e', root, '', 'reply'] in wake['tags'], wake['tags']
    assert any(t[:2] == ['p', owner] for t in wake['tags']), wake['tags']
    # Replacement requires the actual reviewed head; stale writes cannot win.
    disabled = dict(definition, enabled=False)
    changed = call('workflows', 'update', '--channel', channel, '--workflow', workflow,
                   '--yaml', '-', '--if-event', head['event_id'], content=json.dumps(disabled))
    call('workflows', 'update', '--channel', channel, '--workflow', workflow,
         '--yaml', '-', '--if-event', head['event_id'], content=json.dumps(definition), rejected=True)
    assert call('workflows', 'get', '--workflow', workflow)['event_id'] == changed['event_id']
    print(json.dumps({'scheduled_reply_root': root, 'wake_event': wake['id'], 'channel': channel,
                      'invalid_destinations_rejected': 4, 'stale_update_rejected': True,
                      'other_owner_excluded': True, 'workflow_disabled_after_test': workflow}, indent=2))


if __name__ == '__main__':
    main()
