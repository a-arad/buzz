# Slow uploads with short-lived Blossom proofs

The relay verifies upload authority before reading a request body. It captures
one server timestamp at that gate and uses it again when checking the completed
body. A proof valid on arrival can therefore finish a transfer after its expiry.
The proof still binds the signature, upload verb, server and SHA-256. Legacy
creation-age limits remain 600 seconds for images/files and 3600 for videos;
they are evaluated at admission, after actual bytes select the media type.
Download authorization continues to use the current time.

This is the compatible backport of the slow-transfer repair described in
upstream `75e9bef748d2149ce459b14da842e706a51a5f78`. The older verifier has
media-specific age windows, so copying only the newer hash-only post-body check
would widen the image/file window. The captured timestamp preserves those
existing constraints without copying the newer strictness/configuration API.

## Verification

Run the complete media and relay package tests, plus `just ci` and `just test`.
Then run `scripts/test-slow-blossom-upload.py --help` for the live regression
recipe. It requires a disposable local database, Redis and S3 bucket and starts
only the supplied relay binary, with synthetic identities and a scrubbed
environment. Its fifteen checks cover image, file and streamed video uploads:
expired admission, fast upload/download, wrong body hash, legacy age windows,
and expiration during body transfer. HTTP 100 Continue establishes that the
server admitted the request before expiry; authenticated retrieval verifies the
stored bytes. Use `--proof-seconds 60 --body-seconds 70` for client-sized proofs.
The unfixed binary must fail the three slow-transfer cases.

For production, follow [relay publication](relay-publication.md): integrate
through protected main, backport to the installed stable source separately,
and record both revisions and the exact tested container digest. Preserve the
[agent DM policy](agent-dm-policy.md), existing database extensions and recovery
artifacts. Verify database restore and observer enrollment before promotion.
A passing source build alone does not establish deployment compatibility.
