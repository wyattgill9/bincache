#!/usr/bin/env python3
"""End-to-end protocol conformance for a running bincache.

Speaks the wire directly rather than through a client library, because the point is to
assert the exact bytes a Nix client depends on. Each check names the client behaviour it
protects and, where the behaviour comes from Nix's own source, the file that decides it.

Usage:
    conformance.py --base-url http://127.0.0.1:5599 --token <push token> \
        --public-key <name>:<base64>
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import http.client
import io
import sys
import urllib.parse

import zstandard
from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives.asymmetric import ed25519

NIX_BASE32 = "0123456789abcdfghijklmnpqrsvwxyz"


def nix_base32(raw: bytes) -> str:
    """Nix's reversed, little-endian base32 (`printHash32` in libutil/hash.cc)."""
    length = (len(raw) * 8 - 1) // 5 + 1
    out = []
    for n in range(length - 1, -1, -1):
        bit = n * 5
        index, shift = bit // 8, bit % 8
        window = raw[index] >> shift
        if index + 1 < len(raw):
            window |= raw[index + 1] << (8 - shift)
        out.append(NIX_BASE32[window & 0x1F])
    return "".join(out)


def nar_string(value: bytes) -> bytes:
    """Length-prefixed, zero-padded to an 8-byte boundary."""
    padding = (-len(value)) % 8
    return len(value).to_bytes(8, "little") + value + b"\0" * padding


def nar_regular_file(contents: bytes) -> bytes:
    """A minimal but genuine `nix-archive-1` serialization of one regular file."""
    parts = [
        nar_string(b"nix-archive-1"),
        nar_string(b"("),
        nar_string(b"type"),
        nar_string(b"regular"),
        nar_string(b"contents"),
        nar_string(contents),
        nar_string(b")"),
    ]
    return b"".join(parts)


class Failure(Exception):
    pass


class Checks:
    def __init__(self, base_url: str) -> None:
        parsed = urllib.parse.urlparse(base_url)
        self.host = parsed.hostname or "127.0.0.1"
        self.port = parsed.port or 80
        self.passed: list[str] = []
        self.failed: list[str] = []

    def request(
        self,
        method: str,
        target: str,
        body: bytes | None = None,
        headers: dict[str, str] | None = None,
    ) -> tuple[int, dict[str, str], bytes]:
        connection = http.client.HTTPConnection(self.host, self.port, timeout=30)
        try:
            connection.request(method, target, body=body, headers=headers or {})
            response = connection.getresponse()
            payload = response.read()
            return response.status, {k.lower(): v for k, v in response.getheaders()}, payload
        finally:
            connection.close()

    def check(self, name: str, condition: bool, detail: str = "") -> None:
        if condition:
            self.passed.append(name)
            print(f"  ok   {name}")
        else:
            self.failed.append(name)
            print(f"  FAIL {name}{': ' + detail if detail else ''}")

    def equal(self, name: str, actual: object, expected: object) -> None:
        self.check(name, actual == expected, f"got {actual!r}, wanted {expected!r}")


def fingerprint(store_path: str, nar_hash32: str, nar_size: int, references: list[str]) -> bytes:
    """`ValidPathInfo::fingerprint` in nix/src/libstore/path-info.cc."""
    joined = ",".join(sorted(references))
    return f"1;{store_path};sha256:{nar_hash32};{nar_size};{joined}".encode()


def run(base_url: str, token: str, public_key: str) -> int:
    checks = Checks(base_url)
    auth = {"Authorization": f"Bearer {token}"}

    contents = b"bincache conformance payload\n" * 4096
    nar = nar_regular_file(contents)
    nar_hash = hashlib.sha256(nar).digest()
    nar_hash32 = nix_base32(nar_hash)
    path_hash32 = nix_base32(hashlib.sha256(b"conformance-store-path").digest()[:20])
    store_path = f"/nix/store/{path_hash32}-conformance-1.0"

    print("nix-cache-info")
    status, headers, body = checks.request("GET", "/nix-cache-info")
    checks.equal("nix-cache-info returns 200", status, 200)
    text = body.decode()
    checks.check("carries StoreDir", "StoreDir: " in text, text)
    checks.check("carries WantMassQuery", "WantMassQuery: " in text, text)
    checks.check("carries Priority", "Priority: " in text, text)

    print("\nauth")
    status, _, _ = checks.request("PUT", f"/nar/{nar_hash32}.nar", body=b"x")
    checks.equal("an unauthenticated PUT is refused", status, 401)
    status, _, _ = checks.request(
        "PUT", f"/nar/{nar_hash32}.nar", body=b"x", headers={"Authorization": "Bearer wrong"}
    )
    checks.equal("a wrong token is refused", status, 401)

    print("\nupload rejects what it cannot verify")
    # A target naming bytes that are never uploaded, so this holds on a warm store too.
    poison32 = nix_base32(hashlib.sha256(b"bytes that are never uploaded").digest())
    status, _, _ = checks.request("PUT", f"/nar/{poison32}.nar", body=b"not the nar", headers=auth)
    checks.equal("a body that does not match the target hash is refused", status, 400)
    status, _, _ = checks.request("HEAD", f"/nar/{poison32}.nar", headers=auth)
    checks.equal("the refused upload left nothing durable", status, 404)

    status, _, _ = checks.request(
        "PUT", f"/nar/{nar_hash32}.nar.xz", body=b"xz bytes", headers=auth
    )
    checks.equal("a pre-compressed upload is refused", status, 400)

    print("\nupload")
    # `nix copy --to 'http://host?compression=none'` PUTs the NAR, then the narinfo.
    status, _, _ = checks.request("PUT", f"/nar/{nar_hash32}.nar", body=nar, headers=auth)
    checks.equal("the NAR upload is accepted", status, 201)

    status, _, _ = checks.request("HEAD", f"/nar/{nar_hash32}.nar", headers=auth)
    checks.equal(
        "HEAD on the uploaded NAR URL reports it present, so a client skips re-uploading",
        status,
        200,
    )

    narinfo = (
        f"StorePath: {store_path}\n"
        f"URL: nar/{nar_hash32}.nar\n"
        f"Compression: none\n"
        f"FileHash: sha256:{nar_hash32}\n"
        f"FileSize: {len(nar)}\n"
        f"NarHash: sha256:{nar_hash32}\n"
        f"NarSize: {len(nar)}\n"
        f"References: \n"
    ).encode()
    status, _, _ = checks.request("PUT", f"/{path_hash32}.narinfo", body=narinfo, headers=auth)
    checks.equal("the narinfo publish is accepted", status, 201)

    print("\nmetadata plane")
    status, headers, body = checks.request("GET", f"/{path_hash32}.narinfo")
    checks.equal("the published narinfo is served", status, 200)
    checks.equal("narinfo Content-Type", headers.get("content-type"), "text/x-nix-narinfo")
    checks.check("no Content-Encoding on narinfo", "content-encoding" not in headers, str(headers))

    published: dict[str, list[str]] = {}
    for line in body.decode().splitlines():
        if ": " in line:
            key, value = line.split(": ", 1)
            published.setdefault(key, []).append(value)
        elif line.endswith(":"):
            published.setdefault(line[:-1], []).append("")

    checks.equal("StorePath round-trips", published.get("StorePath", [None])[0], store_path)
    checks.equal("NarHash round-trips", published.get("NarHash", [None])[0], f"sha256:{nar_hash32}")
    checks.equal("NarSize round-trips", published.get("NarSize", [None])[0], str(len(nar)))
    checks.equal(
        "the server recompressed to zstd", published.get("Compression", [None])[0], "zstd"
    )
    checks.check("the server signed it", "Sig" in published, str(published.keys()))

    # The strongest available check: a real client accepts a path when any Sig verifies
    # against a key in trusted-public-keys, so this is that decision, made here.
    key_name, key_b64 = public_key.split(":", 1)
    verifier = ed25519.Ed25519PublicKey.from_public_bytes(base64.b64decode(key_b64))
    signed = fingerprint(store_path, nar_hash32, len(nar), [])
    accepted = False
    for line in published.get("Sig", []):
        name, signature = line.split(":", 1)
        if name != key_name:
            continue
        try:
            verifier.verify(base64.b64decode(signature), signed)
            accepted = True
        except InvalidSignature:
            pass
    checks.check(
        "the signature verifies against trusted-public-keys over the canonical fingerprint",
        accepted,
        str(published.get("Sig")),
    )
    checks.check(
        "the four required fields are all present",
        all(k in published for k in ("StorePath", "URL", "NarHash", "NarSize")),
        str(published.keys()),
    )

    head_status, head_headers, head_body = checks.request("HEAD", f"/{path_hash32}.narinfo")
    checks.equal("HEAD on narinfo answers 200", head_status, 200)
    checks.equal(
        "HEAD reuses the body length", head_headers.get("content-length"), str(len(body))
    )
    checks.equal("HEAD writes no body", head_body, b"")

    print("\npayload plane")
    nar_url = "/" + published["URL"][0]
    status, headers, artifact = checks.request("GET", nar_url)
    checks.equal("the artifact is served", status, 200)
    checks.equal("NAR Content-Type", headers.get("content-type"), "application/x-nix-nar")

    # Both of these decide whether a dropped 10 GB transfer resumes or restarts at zero.
    # `maybeRetry` in nix/src/libstore/filetransfer.cc requires the first and refuses the
    # second.
    checks.equal("Accept-Ranges: bytes is advertised", headers.get("accept-ranges"), "bytes")
    checks.check(
        "no Content-Encoding on the NAR", "content-encoding" not in headers, str(headers)
    )
    checks.equal("Content-Length matches the body", int(headers["content-length"]), len(artifact))
    checks.equal("FileSize matches what is served", published["FileSize"][0], str(len(artifact)))
    checks.equal(
        "FileHash names the artifact",
        published["FileHash"][0],
        "sha256:" + nix_base32(hashlib.sha256(artifact).digest()),
    )

    decompressed = zstandard.ZstdDecompressor().stream_reader(io.BytesIO(artifact)).read()
    checks.equal("the artifact decompresses to the uploaded NAR", decompressed, nar)
    checks.equal(
        "the decompressed bytes hash to the published NarHash",
        nix_base32(hashlib.sha256(decompressed).digest()),
        nar_hash32,
    )

    print("\nresume")
    # The only Range form nix emits: a single open-ended one, from
    # CURLOPT_RESUME_FROM_LARGE.
    resume_at = len(artifact) // 2
    status, headers, partial = checks.request(
        "GET", nar_url, headers={"Range": f"bytes={resume_at}-"}
    )
    checks.equal("a resume request answers 206", status, 206)
    checks.equal(
        "Content-Range names the span and the total",
        headers.get("content-range"),
        f"bytes {resume_at}-{len(artifact) - 1}/{len(artifact)}",
    )
    checks.equal("the resumed body is the tail", partial, artifact[resume_at:])
    checks.equal(
        "a resumed response still advertises ranges", headers.get("accept-ranges"), "bytes"
    )

    status, headers, _ = checks.request(
        "GET", nar_url, headers={"Range": f"bytes={len(artifact)}-"}
    )
    checks.equal("a range past the end answers 416", status, 416)
    checks.equal(
        "416 reports the full length", headers.get("content-range"), f"bytes */{len(artifact)}"
    )

    print("\nmisses and malformed input")
    absent = nix_base32(hashlib.sha256(b"never published").digest()[:20])
    status, _, _ = checks.request("GET", f"/{absent}.narinfo")
    checks.equal("an unknown path is a 404", status, 404)
    status, _, _ = checks.request("HEAD", f"/{absent}.narinfo")
    checks.equal("HEAD on an unknown path is a 404", status, 404)
    status, _, _ = checks.request("GET", "/notahash.narinfo")
    checks.equal("a malformed key dies at the socket with 400", status, 400)
    status, _, _ = checks.request("GET", f"/{'e' * 32}.narinfo")
    checks.equal("a key using dropped alphabet letters is a 400", status, 400)
    status, _, _ = checks.request("GET", "/log/whatever.drv")
    checks.equal("an unserved route is a 404", status, 404)

    print("\nidempotence")
    status, _, _ = checks.request("PUT", f"/nar/{nar_hash32}.nar", body=nar, headers=auth)
    checks.equal("re-uploading the same NAR is accepted", status, 201)
    status, _, _ = checks.request("PUT", f"/{path_hash32}.narinfo", body=narinfo, headers=auth)
    checks.equal("re-publishing the same path is accepted", status, 201)
    status, _, again = checks.request("GET", f"/{path_hash32}.narinfo")
    checks.equal("the record is unchanged after a repeat push", again, body)

    print("\nkeep-alive")
    connection = http.client.HTTPConnection(checks.host, checks.port, timeout=30)
    try:
        codes = []
        for _ in range(3):
            connection.request("GET", f"/{path_hash32}.narinfo")
            response = connection.getresponse()
            response.read()
            codes.append(response.status)
        checks.equal("three requests share one connection", codes, [200, 200, 200])
    finally:
        connection.close()

    print(f"\n{len(checks.passed)} passed, {len(checks.failed)} failed")
    for name in checks.failed:
        print(f"  FAILED: {name}")
    return 1 if checks.failed else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", default="http://127.0.0.1:5599")
    parser.add_argument("--token", required=True)
    parser.add_argument(
        "--public-key",
        required=True,
        help="the cache's trusted-public-keys entry, <name>:<base64>",
    )
    args = parser.parse_args()
    return run(args.base_url, args.token, args.public_key)


if __name__ == "__main__":
    sys.exit(main())
