#!/usr/bin/env python3
"""End-to-end proof against a real Nix client.

Builds a store path, pushes it to a fresh bincache with `nix copy --to`, reads the record
back with `nix path-info`, then substitutes it into a separate store with `nix copy --from`.
That last step is the only thing that actually proves the cache works: it makes the client
verify bincache's signature, decompress the artifact, and check `NarHash` before it will
write the path.

The destination must be a store (`--to /some/path`), not a binary cache (`--to file://...`).
A binary cache destination re-uploads without checking anything, so a run that used one
would pass even if the signature were garbage. The negative control below is what keeps
that mistake from going unnoticed again.

The built derivation is unique per run. That is not cosmetic: a Nix client caches negative
narinfo lookups for an hour with a floor `--refresh` cannot lower, so a path this client has
ever missed on would not be re-queried and the run would report a failure that is not one.

Usage:
    e2e-nix.py [--binary target/release/bincache] [--keep]
"""

from __future__ import annotations

import argparse
import json
import pathlib
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

#: Where the run's keys, data directory, and destination store live.
ROOT = pathlib.Path("target/e2e-nix")

#: How long to wait for the server to answer before giving up on it.
STARTUP_TIMEOUT = 20.0


class Failure(Exception):
    pass


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def run(command: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
    finished = subprocess.run(command, capture_output=True, text=True, **kwargs)
    if finished.returncode != 0:
        raise Failure(
            f"{' '.join(command[:3])} ... exited {finished.returncode}\n"
            f"stdout: {finished.stdout.strip()}\nstderr: {finished.stderr.strip()}"
        )
    return finished


def build_unique_path() -> str:
    """A derivation whose output differs every run, so no client cache can shortcut it."""
    marker = f"bincache-e2e-{time.time_ns()}"
    expression = (
        "derivation { "
        f'name = "{marker}"; '
        "system = builtins.currentSystem; "
        'builder = "/bin/sh"; '
        f'args = ["-c" "echo {marker} > $out"]; '
        "}"
    )
    built = run(
        ["nix", "build", "--impure", "--no-link", "--print-out-paths", "--expr", expression]
    )
    return built.stdout.strip().splitlines()[-1]


def wait_for(base_url: str, server: subprocess.Popen[bytes]) -> None:
    deadline = time.monotonic() + STARTUP_TIMEOUT
    while time.monotonic() < deadline:
        if server.poll() is not None:
            raise Failure(f"the server exited early with {server.returncode}")
        try:
            with urllib.request.urlopen(f"{base_url}/nix-cache-info", timeout=1) as response:
                if response.status == 200:
                    return
        except (urllib.error.URLError, ConnectionError, TimeoutError, OSError):
            time.sleep(0.1)
    raise Failure("the server never answered /nix-cache-info")


def check(name: str, condition: bool, detail: str = "") -> bool:
    print(f"  {'ok  ' if condition else 'FAIL'} {name}{'' if condition else ': ' + detail}")
    return condition


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/release/bincache")
    parser.add_argument("--keep", action="store_true", help="leave the run directory in place")
    args = parser.parse_args()

    binary = pathlib.Path(args.binary).resolve()
    if not binary.exists():
        raise Failure(f"{binary} does not exist; run `cargo build --release` first")
    if shutil.which("nix") is None:
        raise Failure("nix is not on PATH, and this script exists to test against it")

    if ROOT.exists():
        shutil.rmtree(ROOT)
    ROOT.mkdir(parents=True)

    print("setting up")
    keygen = run([str(binary), "keygen", "--name", "bincache-e2e-1"])
    (ROOT / "secret.key").write_text(keygen.stdout)
    public_key = keygen.stderr.split("trusted-public-keys entry: ", 1)[1].strip()
    token = run([str(binary), "token"]).stdout.strip()
    (ROOT / "push.token").write_text(token + "\n")
    print(f"  public key {public_key}")

    store_path = build_unique_path()
    print(f"  built {store_path}")
    contents = pathlib.Path(store_path).read_text()

    port = free_port()
    base_url = f"http://127.0.0.1:{port}"
    log = (ROOT / "serve.log").open("wb")
    server = subprocess.Popen(
        [
            str(binary), "serve",
            "--data-dir", str(ROOT / "data"),
            "--secret-key-file", str(ROOT / "secret.key"),
            "--push-token-file", str(ROOT / "push.token"),
            "--listen", f"127.0.0.1:{port}",
            "--shards", "2",
        ],
        stdout=log,
        stderr=subprocess.STDOUT,
    )

    passed = True
    try:
        wait_for(base_url, server)
        print(f"  serving on {base_url}")

        print("\npush, with a real client")
        run(["nix", "copy", "--to", f"http://bincache:{token}@127.0.0.1:{port}?compression=none",
             store_path])
        passed &= check("nix copy --to accepted the path", True)

        print("\nread the record back, with a real client")
        info = run(["nix", "path-info", "--store", base_url, "--json", "--json-format", "1",
                    store_path])
        record = json.loads(info.stdout)[store_path]
        passed &= check("the client parsed the narinfo", record is not None, info.stdout)
        passed &= check("the server recompressed to zstd",
                        record["compression"] == "zstd", str(record.get("compression")))
        passed &= check("the record carries bincache's signature",
                        any(sig.startswith("bincache-e2e-1:") for sig in record["signatures"]),
                        str(record["signatures"]))

        print("\nsubstitute into a separate store, signatures checked")
        destination = ROOT / "dest"
        run(["nix", "copy", "--from", base_url, "--to", str(destination.resolve()),
             store_path, "--option", "trusted-public-keys", public_key])
        passed &= check("the client verified the signature and unpacked the NAR", True)

        landed = destination / store_path.lstrip("/")
        passed &= check("the path landed in the destination store", landed.exists(), str(landed))
        passed &= check("its contents survived the round trip through zstd",
                        landed.read_text() == contents if landed.exists() else False)

        # Negative control. Without this, every check above would still pass if the client
        # were ignoring signatures entirely, and the run would prove nothing about them.
        print("\nthe signature check is real")
        stranger = run([str(binary), "keygen", "--name", "not-bincache-1"])
        wrong_key = stranger.stderr.split("trusted-public-keys entry: ", 1)[1].strip()
        refused = subprocess.run(
            ["nix", "copy", "--from", base_url, "--to", str((ROOT / "refused").resolve()),
             store_path, "--option", "trusted-public-keys", wrong_key],
            capture_output=True,
            text=True,
        )
        passed &= check("a client trusting a different key refuses the path",
                        refused.returncode != 0,
                        "the copy succeeded, so nothing above proved anything about signatures")
        # Nix words this differently depending on the destination store, so match the
        # subject rather than a phrase: pinning the exact sentence would break on an
        # upstream reword and say nothing about bincache.
        blamed_the_signature = "signature" in refused.stderr or "public key" in refused.stderr
        passed &= check("and refuses it for the right reason", blamed_the_signature,
                        refused.stderr.strip()[-200:])
    finally:
        server.terminate()
        server.wait(timeout=10)
        log.close()
        if not args.keep:
            print(f"\nrun directory left at {ROOT} (server log inside)")

    print("\nPASS" if passed else "\nFAIL")
    return 0 if passed else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Failure as failure:
        print(f"error: {failure}", file=sys.stderr)
        sys.exit(1)
