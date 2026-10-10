#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["coincurve==21.0.0"]
# ///
r"""Probe Blossom admission-time expiry through a disposable local relay.

Requires an already-built relay, a disposable local PostgreSQL database whose
name includes slow_blossom, disposable local Redis, and a pre-created local S3
bucket whose name includes slow-blossom. The relay auto-migrates that database.
Only random test identities and explicit local-development S3 credentials are
used; inherited Buzz credentials/configuration are not passed to the relay.
Services, database and bucket remain caller-owned; this script stops its relay.

Example (substitute ports/paths for your disposable services):
  uv run scripts/test-slow-blossom-upload.py --relay-binary target/debug/buzz-relay \
    --database-url postgresql://postgres:postgres@127.0.0.1:55432/slow_blossom_test \
    --redis-url redis://127.0.0.1:56379/0 --s3-endpoint http://127.0.0.1:59000 \
    --log-dir /tmp/slow-blossom-candidate

Defaults: 2-second proof, 3-second body. Add --proof-seconds 60 --body-seconds 70
for the actual client proof lifetime. Each selected kind runs an expired-header
rejection, a fast upload/retrieval control, a wrong-body-hash rejection, a
601-second-old proof control, and a slow upload/retrieval. Old proofs retain
the 600-second image/file and 3600-second video creation-age limits. The slow
request must receive HTTP 100 Continue before expiry (the relay has admitted
the request and polled its body), and its last
byte is sent after expiry. Run against a baseline too: only slow uploads should
fail there. Output is JSON Lines, with a final summary and nonzero failure exit.

The video fixture mirrors e2e_media_video.rs's structural MP4 fixture. It covers
the streaming upload/validation/storage path, not playable video or thumbnails.
"""

import argparse
import base64
import hashlib
import http.client
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
import threading
import time
from urllib.parse import urlparse
import uuid
import zlib

from coincurve import PrivateKey


MAX_RESPONSE = 2 * 1024 * 1024
MAX_LOG = 1024 * 1024


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def public(key):
    return key.public_key_xonly.format().hex()


def auth_header(key, verb, digest, host, expires, created_age=0):
    tags = [["t", verb], ["x", digest], ["server", host], ["expiration", str(expires)]]
    record = [0, public(key), int(time.time()) - created_age, 24242, tags, f"{verb} slow-upload test"]
    event_id = hashlib.sha256(json.dumps(record, separators=(",", ":")).encode()).digest()
    signed = dict(id=event_id.hex(), pubkey=record[1], created_at=record[2], kind=24242,
                  tags=tags, content=record[5], sig=key.sign_schnorr(event_id).hex())
    return "Nostr " + base64.urlsafe_b64encode(json.dumps(signed).encode()).decode().rstrip("=")


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def fixture(kind):
    if kind == "file":
        return json.dumps({"probe": "slow Blossom", "nonce": uuid.uuid4().hex}).encode(), "application/octet-stream"
    if kind == "image":
        def chunk(name, data):
            return (struct.pack(">I", len(data)) + name + data
                    + struct.pack(">I", zlib.crc32(name + data)))
        pixels = b"".join(b"\0" + os.urandom(16 * 3) for _ in range(16))
        image = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 16, 16, 8, 2, 0, 0, 0))
                 + chunk(b"IDAT", zlib.compress(pixels)) + chunk(b"IEND", b""))
        return image, "image/png"
    return video_fixture(), "video/mp4"


def video_fixture():
    """Port the existing Rust E2E fixture, retaining metadata-free box contents."""
    def integers(*values):
        return struct.pack(">" + "I" * len(values), *values)

    def box(name, data):
        return integers(8 + len(data)) + name + data

    # Vary geometry so repeat runs do not rely on an existing stored blob.
    width = 320 + 2 * (uuid.uuid4().int % 100)
    matrix = integers(0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000)
    ftyp = box(b"ftyp", b"isom" + integers(0) + b"isom")
    mvhd = box(b"mvhd", integers(0, 0, 0, 1000, 1000, 0x10000)
               + b"\x01\0" + bytes(10) + matrix + bytes(24) + integers(2))
    tkhd = box(b"tkhd", integers(3, 0, 0, 1, 0, 1000) + bytes(16)
               + matrix + integers(width << 16, 240 << 16))
    mdhd = box(b"mdhd", integers(0, 0, 0, 1000, 1000) + bytes(4))
    hdlr = box(b"hdlr", integers(0, 0) + b"vide" + bytes(12) + b"VideoHandler\0")
    vmhd = box(b"vmhd", integers(1) + bytes(8))
    dinf = box(b"dinf", box(b"dref", integers(0, 1) + box(b"url ", integers(1))))
    avcc = box(b"avcC", bytes.fromhex("0142001effe10000010000"))
    avc1 = box(b"avc1", bytes(6) + b"\0\x01" + bytes(16)
               + struct.pack(">HH", width, 240) + integers(0x480000, 0x480000, 0)
               + b"\0\x01" + bytes(32) + b"\0\x18\xff\xff" + avcc)
    stbl = box(b"stbl", box(b"stsd", integers(0, 1) + avc1)
               + box(b"stts", integers(0, 1, 1, 1000))
               + box(b"stsc", integers(0, 1, 1, 1, 1))
               + box(b"stsz", integers(0, 0, 1, 0))
               + box(b"stco", integers(0, 1, 28)))
    mdia = box(b"mdia", mdhd + hdlr + box(b"minf", vmhd + dinf + stbl))
    return ftyp + box(b"moov", mvhd + box(b"trak", tkhd + mdia)) + box(b"mdat", b"")


class Relay:
    def __init__(self, args):
        self.scratch = tempfile.TemporaryDirectory(prefix="buzz-slow-blossom-")
        self.port = free_port()
        self.host = f"127.0.0.1:{self.port}"
        self.url = "http://" + self.host
        self.log_path = args.log_dir / "relay.log"
        environment = {name: os.environ[name] for name in ("PATH", "HOME") if name in os.environ}
        environment.update(DATABASE_URL=args.database_url, REDIS_URL=args.redis_url,
                           BUZZ_BIND_ADDR=self.host, RELAY_URL="ws://" + self.host,
                           BUZZ_HEALTH_PORT=str(free_port()), BUZZ_METRICS_PORT=str(free_port()),
                           BUZZ_AUTO_MIGRATE="true", BUZZ_RELAY_PRIVATE_KEY=PrivateKey().secret.hex(),
                           BUZZ_REQUIRE_RELAY_MEMBERSHIP="false", BUZZ_REQUIRE_AUTH_TOKEN="true",
                           BUZZ_ALLOW_NIP98_AUTH="true", RUST_LOG="warn",
                           BUZZ_S3_ENDPOINT=args.s3_endpoint, BUZZ_S3_BUCKET=args.s3_bucket,
                           BUZZ_S3_ACCESS_KEY=args.s3_access_key, BUZZ_S3_SECRET_KEY=args.s3_secret_key,
                           BUZZ_S3_REGION="us-east-1", BUZZ_S3_ADDRESSING_STYLE="path")
        self.process = subprocess.Popen([str(args.relay_binary)], cwd=self.scratch.name,
                                        env=environment, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.log_thread = threading.Thread(target=self.capture_log, daemon=True)
        self.log_thread.start()
        try:
            deadline = time.monotonic() + 45
            while time.monotonic() < deadline:
                check(self.process.poll() is None, f"relay exited; inspect {self.log_path}")
                try:
                    if self.get("/health", timeout=1)[0] == 200:
                        return
                except (OSError, http.client.HTTPException):
                    pass
                time.sleep(0.2)
            raise TimeoutError(f"relay startup timed out; inspect {self.log_path}")
        except BaseException:
            self.close()
            raise

    def capture_log(self):
        with self.log_path.open("wb") as output:
            remaining = MAX_LOG
            while chunk := self.process.stdout.read(4096):
                if remaining:
                    output.write(chunk[:remaining])
                    output.flush()
                    remaining = max(0, remaining - len(chunk))

    def close(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
        self.log_thread.join(timeout=5)
        check(not self.log_thread.is_alive(), "relay log reader did not stop")
        self.process.stdout.close()
        self.scratch.cleanup()

    def get(self, path, auth=None, timeout=15):
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=timeout)
        try:
            connection.request("GET", path, headers={"Authorization": auth} if auth else {})
            response = connection.getresponse()
            data = response.read(MAX_RESPONSE + 1)
            check(len(data) <= MAX_RESPONSE, "response exceeded size limit")
            return response.status, data
        finally:
            connection.close()

    def upload(self, key, data, mime, lifetime, duration=0, wrong_hash=False, expired=False, created_age=0):
        digest = hashlib.sha256(data).hexdigest()
        claimed = "0" * 64 if wrong_hash else digest
        expires = int(time.time()) + (-1 if expired else lifetime)
        token = auth_header(key, "upload", claimed, self.host, expires, created_age)
        with socket.create_connection(("127.0.0.1", self.port), timeout=15) as connection:
            headers = (f"PUT /upload HTTP/1.1\r\nHost: {self.host}\r\nAuthorization: {token}\r\n"
                       f"Content-Type: {mime}\r\nContent-Length: {len(data)}\r\n"
                       f"X-SHA-256: {claimed}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n")
            connection.sendall(headers.encode())
            # Read exactly one header block, without consuming any final response.
            head = bytearray()
            while not head.endswith(b"\r\n\r\n"):
                part = connection.recv(1)
                check(part, "connection closed before admission response")
                head.extend(part)
                check(len(head) <= 16384, "admission response headers exceeded limit")
            status = int(head.split(b" ", 2)[1])
            if expired:
                check(status == 401, f"expired proof: expected 401 before body, got {status}")
                return {"http_status": status, "body_bytes_sent": 0}
            check(status == 100, f"expected admission 100 Continue, got {status}")
            admitted = time.time()
            check(admitted < expires, "proof expired before admission; test timing invalid")
            start = time.monotonic()
            chunks = 32
            for index in range(chunks):
                target = start + duration * index / (chunks - 1)
                delay = target - time.monotonic()
                if delay > 0:
                    time.sleep(delay)
                connection.sendall(data[len(data) * index // chunks:len(data) * (index + 1) // chunks])
            finished = time.time()
            if duration:
                check(finished > expires, "body finished before proof expiry; test timing invalid")
            response = http.client.HTTPResponse(connection)
            response.begin()
            body = response.read(MAX_RESPONSE + 1)
            check(len(body) <= MAX_RESPONSE, "upload response exceeded size limit")
            expected_status = 401 if wrong_hash or (created_age > 600 and mime != "video/mp4") else 200
            check(response.status == expected_status,
                  f"expected {expected_status}, got {response.status}: {body[:500]!r}")
            facts = dict(http_status=response.status, body_bytes_sent=len(data),
                         body_seconds=round(time.monotonic() - start, 3),
                         admitted_before_expiry_seconds=round(expires - admitted, 3),
                         finished_after_expiry_seconds=round(finished - expires, 3))
            if expected_status == 401:
                return facts
            descriptor = json.loads(body)
            check(descriptor.get("sha256") == digest, "descriptor hash differs from upload")
            check(descriptor.get("size") == len(data), "descriptor size differs from upload")
            check(descriptor.get("type") == mime, f"unexpected descriptor MIME: {descriptor.get('type')}")
            download = urlparse(descriptor["url"])
            check(download.scheme == "http" and download.netloc == self.host,
                  "descriptor points outside the isolated relay")
            check(not download.query and not download.fragment and download.path.startswith("/media/" + digest),
                  "unexpected download path")
            check(self.get(download.path)[0] == 401, "unauthenticated download should fail")
            read_auth = auth_header(key, "get", digest, self.host, int(time.time()) + 60)
            read_status, payload = self.get(download.path, read_auth)
            check(read_status == 200, f"authenticated retrieval failed: {read_status}")
            check(hashlib.sha256(payload).hexdigest() == digest, "downloaded payload hash mismatch")
            facts.update(sha256=digest, authenticated_retrieval="pass", unauthenticated_retrieval="rejected")
            return facts


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--relay-binary", required=True, type=Path)
    parser.add_argument("--database-url", required=True)
    parser.add_argument("--redis-url", required=True)
    parser.add_argument("--s3-endpoint", required=True)
    parser.add_argument("--s3-bucket", default="buzz-slow-blossom-test")
    parser.add_argument("--s3-access-key", default="minioadmin", help="disposable local S3 credential only")
    parser.add_argument("--s3-secret-key", default="minioadmin", help="disposable local S3 credential only")
    parser.add_argument("--log-dir", required=True, type=Path)
    parser.add_argument("--proof-seconds", type=int, default=2)
    parser.add_argument("--body-seconds", type=float, default=3)
    parser.add_argument("--kinds", nargs="+", choices=("image", "file", "video"), default=["image", "file", "video"])
    args = parser.parse_args()
    for name, value, schemes in (("database", args.database_url, ("postgres", "postgresql")),
                                 ("Redis", args.redis_url, ("redis",)),
                                 ("S3", args.s3_endpoint, ("http",))):
        parsed = urlparse(value)
        if parsed.hostname not in ("localhost", "127.0.0.1", "::1") or parsed.scheme not in schemes:
            parser.error(f"{name} must use a disposable local service")
    if "slow_blossom" not in urlparse(args.database_url).path or "slow-blossom" not in args.s3_bucket:
        parser.error("use a slow_blossom test database and slow-blossom test bucket")
    if not 2 <= args.proof_seconds <= 120 or not args.proof_seconds + 1 <= args.body_seconds <= 180:
        parser.error("proof must be 2–120 seconds; body must exceed proof by >=1 second and be <=180")
    args.relay_binary = args.relay_binary.resolve(strict=True)
    args.log_dir.mkdir(parents=True, exist_ok=True)
    if (args.log_dir / "relay.log").exists():
        parser.error("relay.log already exists; choose a fresh log directory")
    with args.relay_binary.open("rb") as binary:
        binary_hash = hashlib.file_digest(binary, "sha256").hexdigest()
    key = PrivateKey()
    results = []
    relay = Relay(args)
    try:
        for kind in dict.fromkeys(args.kinds):
            data, mime = fixture(kind)
            for case, options in (
                ("expired_before_body", dict(lifetime=60, expired=True)),
                ("fast_control", dict(lifetime=60)),
                ("wrong_body_hash", dict(lifetime=60, wrong_hash=True)),
                ("old_proof_age_limit", dict(lifetime=60, created_age=601)),
                ("expires_during_body", dict(lifetime=args.proof_seconds, duration=args.body_seconds)),
            ):
                result = dict(kind=kind, case=case)
                try:
                    result.update(result="pass", **relay.upload(key, data, mime, **options))
                except (AssertionError, OSError, ValueError, KeyError, http.client.HTTPException) as error:
                    result.update(result="fail", error=str(error))
                results.append(result)
                print(json.dumps(result), flush=True)
    finally:
        relay.close()
    failures = sum(result["result"] == "fail" for result in results)
    print(json.dumps(dict(result="fail" if failures else "pass", cases=len(results), failures=failures,
                          relay_binary_sha256=binary_hash, proof_seconds=args.proof_seconds,
                          body_seconds=args.body_seconds, log_path=str(relay.log_path))), flush=True)
    return int(bool(failures))


if __name__ == "__main__":
    raise SystemExit(main())
