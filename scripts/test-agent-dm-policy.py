#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["coincurve==21.0.0", "httpx==0.28.1", "psycopg[binary]==3.2.9", "websocket-client==1.8.0"]
# ///
"""Exercise DM policy through authenticated HTTP and WS on an isolated relay.

Requires disposable local Postgres/Redis and a built relay; never uses fleet keys.
The database name must contain dm_policy. Run against stock and patched binaries
to establish that policy assertions fail without the production guards.
"""

import argparse
import base64
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
from urllib.parse import urlparse
import uuid

from coincurve import PrivateKey
import httpx
import psycopg
import websocket


def public(key):
    return key.public_key_xonly.format().hex()


def event(key, kind, tags=(), content=""):
    record = [0, public(key), int(time.time()), kind, list(tags), content]
    digest = hashlib.sha256(json.dumps(record, separators=(",", ":"), ensure_ascii=False).encode()).digest()
    return dict(id=digest.hex(), pubkey=record[1], created_at=record[2], kind=kind,
                tags=record[4], content=content, sig=key.sign_schnorr(digest).hex())


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


class Relay:
    def __init__(self, args, humans, enabled):
        self.scratch = tempfile.TemporaryDirectory(prefix="buzz-dm-policy-")
        self.url = f"http://127.0.0.1:{args.relay_port}"
        self.ws = self.url.replace("http:", "ws:")
        environment = {key: os.environ[key] for key in ("PATH", "HOME") if key in os.environ}
        environment.update(DATABASE_URL=args.database_url, REDIS_URL=args.redis_url,
                           BUZZ_BIND_ADDR=self.url.removeprefix("http://"),
                           RELAY_URL=self.ws, BUZZ_HEALTH_PORT=str(free_port()),
                           BUZZ_METRICS_PORT=str(free_port()), BUZZ_AUTO_MIGRATE="true",
                           BUZZ_RELAY_PRIVATE_KEY=PrivateKey().secret.hex(),
                           BUZZ_REQUIRE_RELAY_MEMBERSHIP="false", BUZZ_REQUIRE_AUTH_TOKEN="true",
                           BUZZ_ALLOW_NIP98_AUTH="true", RUST_LOG="warn",
                           BUZZ_S3_ENDPOINT=args.s3_endpoint, BUZZ_S3_BUCKET="buzz-dm-policy-test",
                           BUZZ_S3_ACCESS_KEY="minioadmin", BUZZ_S3_SECRET_KEY="minioadmin")
        if enabled:
            environment["BUZZ_DM_HUMAN_PUBKEYS"] = ",".join(map(public, humans))
        self.log = open(args.log_dir / ("enabled.log" if enabled else "disabled.log"), "w")
        self.process = subprocess.Popen([str(args.relay_binary)], cwd=self.scratch.name,
                                        env=environment, stdout=self.log, stderr=subprocess.STDOUT)
        try:
            for _ in range(150):
                if self.process.poll() is not None:
                    raise RuntimeError(f"relay exited; inspect {self.log.name}")
                try:
                    if httpx.get(self.url + "/health", timeout=1).is_success:
                        return
                except httpx.HTTPError:
                    pass
                time.sleep(0.2)
            raise TimeoutError(f"relay startup timed out; inspect {self.log.name}")
        except BaseException:
            self.close()
            raise

    def close(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
        self.log.close()
        self.scratch.cleanup()

    def http(self, key, signed, route="events"):
        body = json.dumps(signed, separators=(",", ":")).encode()
        url = self.url + "/" + route
        auth = event(key, 27235, [["u", url], ["method", "POST"],
                                  ["payload", hashlib.sha256(body).hexdigest()]])
        token = base64.b64encode(json.dumps(auth).encode()).decode()
        response = httpx.post(url, content=body, headers={"Authorization": "Nostr " + token,
                              "Content-Type": "application/json"}, timeout=10)
        return response.json()

    def send(self, key, kind, tags=(), content="", allowed=True, reason=None):
        signed = event(key, kind, tags, content or uuid.uuid4().hex)
        response = self.http(key, signed)
        assert response.get("accepted", False) is allowed, (kind, allowed, response)
        if reason:
            assert reason in response.get("message", response.get("error", "")), response
        return signed, response

    def ws_send(self, actor, signed, allowed):
        with closing(websocket.create_connection(self.ws, timeout=10)) as connection:
            challenge = json.loads(connection.recv())
            assert challenge[0] == "AUTH", challenge
            auth = event(actor, 22242, [["relay", self.ws], ["challenge", challenge[1]]])
            connection.send(json.dumps(["AUTH", auth]))
            def ok(identifier):
                for _ in range(20):
                    message = json.loads(connection.recv())
                    if message[:2] == ["OK", identifier]:
                        return message
                raise AssertionError("missing OK")
            assert ok(auth["id"])[2] is True
            connection.send(json.dumps(["EVENT", signed]))
            result = ok(signed["id"])
            assert result[2] is allowed, result
            if not allowed:
                assert "restricted:" in result[3], result
            return signed

    def wrap(self, actor, signer, recipients, allowed):
        signed = event(signer, 1059, [["p", public(key)] for key in recipients], uuid.uuid4().hex)
        return self.ws_send(actor, signed, allowed)


def dm(relay, actor, participants):
    _, response = relay.send(actor, 41010, [["p", public(key)] for key in participants])
    return json.loads(response["message"].removeprefix("response:"))["channel_id"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--relay-binary", required=True, type=Path)
    parser.add_argument("--database-url", required=True)
    parser.add_argument("--redis-url", required=True)
    parser.add_argument("--s3-endpoint", required=True)
    parser.add_argument("--log-dir", required=True, type=Path)
    args = parser.parse_args()
    args.relay_port = free_port()
    for value in (args.database_url, args.redis_url, args.s3_endpoint):
        assert urlparse(value).hostname in ("localhost", "127.0.0.1", "::1"), "local test services only"
    assert "dm_policy" in urlparse(args.database_url).path, "use a disposable dm_policy database"
    args.relay_binary = args.relay_binary.resolve(strict=True)
    args.log_dir.mkdir(parents=True, exist_ok=True)
    human, human2, agent, agent2 = [PrivateKey() for _ in range(4)]
    disabled = Relay(args, [human, human2], False)
    try:
        old_dm = dm(disabled, agent, [agent2])
        old_event, _ = disabled.send(agent, 9, [["h", old_dm]], "retained history")
        disabled.wrap(agent, PrivateKey(), [agent2], True)
    finally:
        disabled.close()
    enabled = Relay(args, [human, human2], True)
    rejected = []
    try:
        for actor, peers in [(agent, [agent2]), (human, [agent, agent2])]:
            signed, _ = enabled.send(actor, 41010, [["p", public(key)] for key in peers],
                                     allowed=False, reason="agent-to-agent DMs are disabled")
            rejected.append(signed)
        rejected.append(enabled.ws_send(agent, event(agent, 41010, [["p", public(agent2)]], uuid.uuid4().hex), False))
        rejected.append(enabled.ws_send(agent, event(agent, 9, [["h", old_dm]], uuid.uuid4().hex), False))
        pair = dm(enabled, human, [agent])
        enabled.ws_send(agent, event(agent, 9, [["h", pair]], "WS human-agent control"), True)
        for actor in (human, agent):
            enabled.send(actor, 9, [["h", pair]], "human-agent conversation")
        dm(enabled, human, [human2])
        dm(enabled, agent, [human, human2])
        for kind, tags in [(41011, [["h", pair], ["p", public(agent2)]]),
                           (9, [["h", old_dm]]),
                           (7, [["e", old_event["id"]], ["p", public(agent)]]),
                           (9000, [["h", pair], ["p", public(agent2)]])]:
            signed, _ = enabled.send(agent, kind, tags, allowed=False, reason=("DM participants are immutable" if kind == 9000
                                                           else "agent-to-agent DMs are disabled"))
            rejected.append(signed)
        expanded = dm(enabled, human, [agent])
        enabled.send(human, 41011, [["h", expanded], ["p", public(human2)]])
        enabled.send(agent, 41012, [["h", old_dm]])
        for visibility in ("open", "private"):
            channel = str(uuid.uuid4())
            tags = [["h", channel], ["name", "dm-policy-control-" + channel],
                    ["channel_type", "stream"], ["visibility", visibility]]
            enabled.send(agent, 9007, tags)
            enabled.send(agent, 9000, [["h", channel], ["p", public(agent2)]])
            enabled.send(agent2, 9, [["h", channel]], "shared channel control")
        signed, _ = enabled.send(agent, 9007, [["h", str(uuid.uuid4())], ["name", "bypass"],
                                ["channel_type", "dm"], ["visibility", "private"]],
                                allowed=False, reason="create DMs through the DM command")
        rejected.append(signed)
        for signer in (PrivateKey(), human):
            rejected.append(enabled.wrap(agent, signer, [agent2], False))
        rejected.append(enabled.wrap(agent, PrivateKey(), [human, agent2], False))
        rejected.append(enabled.wrap(agent, PrivateKey(), [], False))
        enabled.wrap(agent, PrivateKey(), [human], True)
        enabled.wrap(agent, PrivateKey(), [agent], True)
        enabled.wrap(human, PrivateKey(), [agent], True)
        history = enabled.http(agent, [{"ids": [old_event["id"]]}], "query")
        assert old_event["id"] in json.dumps(history), history
        with psycopg.connect(args.database_url) as connection:
            for signed in rejected:
                count = connection.execute("SELECT count(*) FROM events WHERE id = %s",
                                           (bytes.fromhex(signed["id"]),)).fetchone()[0]
                assert count == 0, ("rejected event persisted", signed["id"])
        print(json.dumps({"result": "pass", "rejected_events_absent": len(rejected),
                          "http_auth": "NIP-98", "websocket_auth": "NIP-42",
                          "policy_disabled_control": "pass", "human_and_channel_controls": "pass"}))
    finally:
        enabled.close()


if __name__ == "__main__":
    main()
