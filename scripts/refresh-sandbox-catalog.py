"""Re-resolve the curated sandbox images in src/sandbox_catalog.json to current digests.

Each entry keeps a human `tag` (for example `rust:1.90-slim-bookworm`); this script asks the
registry for that tag's current multi-platform index digest and pins `image` to it, without
pulling the image. Review the diff before committing: a new digest is new code that the
sandboxed verifier will run.

  python scripts/refresh-sandbox-catalog.py           # rewrite the catalog
  python scripts/refresh-sandbox-catalog.py --check   # report drift, exit 1 if any
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

CATALOG = Path(__file__).resolve().parent.parent / "src" / "sandbox_catalog.json"


def resolve(docker: str, tag: str) -> str:
    output = subprocess.run(
        [docker, "buildx", "imagetools", "inspect", tag, "--format", "{{json .Manifest}}"],
        check=True,
        capture_output=True,
        text=True,
        timeout=120,
    ).stdout
    digest = json.loads(output)["digest"]
    if not digest.startswith("sha256:") or len(digest) != 71:
        raise ValueError(f"unexpected digest for {tag}: {digest}")
    repository = tag.rsplit(":", 1)[0]
    return f"{repository}@{digest}"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true", help="report drift without writing")
    parser.add_argument("--docker", default="docker", help="Docker CLI to use")
    args = parser.parse_args()

    entries = json.loads(CATALOG.read_text(encoding="utf-8"))
    drift = []
    for entry in entries:
        pinned = resolve(args.docker, entry["tag"])
        if pinned != entry["image"]:
            drift.append((entry["id"], entry["image"], pinned))
            entry["image"] = pinned
    for entry_id, old, new in drift:
        print(f"{entry_id}: {old} -> {new}")
    if not drift:
        print("catalog is current")
        return 0
    if args.check:
        return 1
    CATALOG.write_text(json.dumps(entries, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"updated {len(drift)} entr{'y' if len(drift) == 1 else 'ies'} in {CATALOG.name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
