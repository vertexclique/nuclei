#!/usr/bin/env python3
"""Builds the site into site/: the pages, the API reference and the search index.

From the repository root:

    python3 site/build.py              # the site, to preview or deploy
    python3 site/build.py --examples   # and every Learn example: built, linted and run

It runs rustdoc for the target the reference is built for (x86_64-unknown-linux-gnu, default
features) and writes, none of it committed:

    site/api/          that rustdoc output, the API reference the pages and the search link to
    site/index.html    the landing page, from site/src/index.html (build_pages.py)
    site/learn/        the Learn pages, from site/src/learn/ and site/examples/ (build_pages.py)
    site/search.json   the index of the search (build_search.py)

A local preview is any static server over site/, for example `python3 -m http.server -d site`.

With --examples it also checks every program the pages show, the crate in site/examples/:
rustfmt finds nothing to change, clippy no warning, each program runs to its end with exit
status 0 on io_uring and on epoll (on Linux; elsewhere on the platform's backend), printing
exactly the output its page shows where the page shows one, and its test files pass on each. An example named in the crate's
`package.metadata.learn.build-only` runs forever by design (a server); it is built and linted,
not run. The Pages workflow runs this before every deploy and on every pull request.
"""

import argparse
import os
import platform
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_pages  # noqa: E402
import build_search  # noqa: E402

ROOT = build_pages.ROOT
EXAMPLES = build_pages.EXAMPLES
TARGET = ROOT / "target" / "learn"
RUN_LIMIT = 60


def fail(message):
    sys.exit(f"build: {message}")


def cargo(subcommand, *args):
    command = ["cargo", subcommand, "--manifest-path", str(EXAMPLES / "Cargo.toml"), *args]
    print("build: " + " ".join(command), file=sys.stderr)
    if subprocess.run(command, cwd=ROOT).returncode != 0:
        fail(f"cargo {subcommand} failed on the Learn examples")


def run_examples(outputs):
    cargo("fmt", "--check")
    cargo("clippy", "--all-targets", "--target-dir", str(TARGET), "--", "-D", "warnings")
    cargo("build", "--bins", "--tests", "--target-dir", str(TARGET))
    manifest = tomllib.loads((EXAMPLES / "Cargo.toml").read_text())
    build_only = set(manifest.get("package", {}).get("metadata", {}).get("learn", {}).get("build-only", []))
    names = sorted(p.stem for p in (EXAMPLES / "src" / "bin").glob("*.rs"))
    if build_only - set(names):
        fail(f"build-only names no example: {', '.join(sorted(build_only - set(names)))}")
    backends = ["io_uring", "epoll"] if platform.system() == "Linux" else [None]
    ran = 0
    for name in names:
        if name in build_only:
            continue
        for backend in backends:
            env = dict(os.environ)
            if backend:
                env["NUCLEI_BACKEND"] = backend
            # Each run starts in an empty directory of its own: an example that writes files
            # writes them there.
            with tempfile.TemporaryDirectory(prefix=f"learn-{name}-") as cwd:
                try:
                    done = subprocess.run([str(TARGET / "debug" / name)], cwd=cwd, env=env, capture_output=True,
                                          text=True, timeout=RUN_LIMIT)
                except subprocess.TimeoutExpired:
                    fail(f"{name} on {backend or 'the default backend'} did not end within {RUN_LIMIT} seconds")
            where = f"{name} on {backend or 'the default backend'}"
            if done.returncode != 0:
                fail(f"{where} exited with {done.returncode}:\n{done.stdout}{done.stderr}")
            if name in outputs and done.stdout.strip("\n") != outputs[name]:
                fail(f"{where} printed\n{done.stdout}\nwhere its page shows\n{outputs[name]}")
            ran += 1
    # The test files the pages show, on each backend.
    for backend in backends:
        env = dict(os.environ)
        if backend:
            env["NUCLEI_BACKEND"] = backend
        command = ["cargo", "test", "--manifest-path", str(EXAMPLES / "Cargo.toml"), "--tests", "--target-dir", str(TARGET)]
        if subprocess.run(command, cwd=ROOT, env=env).returncode != 0:
            fail(f"the Learn test files failed on {backend or 'the default backend'}")
    print(f"build: {len(names)} Learn programs and their test files built and linted; {ran} program runs "
          f"ended well and the tests passed ({', '.join(b or 'default' for b in backends)}; {len(build_only)} built only)")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--examples", action="store_true", help="also build, lint and run every Learn example")
    args = parser.parse_args()
    doc_dir = build_search.run_rustdoc()
    doc = build_search.DocCrate(doc_dir)
    api = build_search.api_entries(doc)
    build_search.publish_api(doc_dir)
    # Every path a page may link by: each item and member, and each re-export of an item with
    # its members under it (nuclei::Builder::build for nuclei::runtime::Builder::build).
    aliases = {entry["p"]: entry.get("a", []) for entry in api if "#" not in entry["u"]}
    urls = {}
    for entry in api:
        owner, _, member = entry["p"].rpartition("::")
        paths = [entry["p"], *entry.get("a", [])]
        if "#" in entry["u"]:
            paths += [f"{alias}::{member}" for alias in aliases.get(owner, [])]
        for path in paths:
            urls.setdefault(path, (entry["u"], entry["s"]))
    outputs = build_pages.build(urls)
    build_search.write_index(api)
    if args.examples:
        run_examples(outputs)


if __name__ == "__main__":
    main()
