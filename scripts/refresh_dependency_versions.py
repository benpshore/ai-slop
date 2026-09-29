"""Refresh repository extractor identities from the root package's lock entries.

This is an update-time tool, not a build script: downstream Cargo resolution
must not be inferred from a library package's bundled Cargo.lock.
"""

from __future__ import annotations

import argparse
import json
import re
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def root_versions(root: Path) -> dict[str, str]:
    lock = tomllib.loads((root / "Cargo.lock").read_text())
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    packages = lock["package"]
    package = next(p for p in packages if p["name"] == manifest["package"]["name"])
    versions = {}
    for dependency in package["dependencies"]:
        parts = dependency.split()
        name = parts[0]
        candidates = [
            p["version"]
            for p in packages
            if p["name"] == name and (len(parts) == 1 or p["version"] == parts[1])
        ]
        if len(candidates) != 1:
            raise ValueError(f"ambiguous root dependency: {dependency}")
        versions[name] = candidates[0]
    return versions


def replace_once(text: str, pattern: str, version: str) -> str:
    updated, count = re.subn(pattern, lambda match: match[1] + version + match[2], text)
    if count != 1:
        raise ValueError(f"expected one version identity for {pattern!r}, found {count}")
    return updated


def refreshed_files(root: Path) -> dict[Path, str]:
    versions = root_versions(root)
    native = json.loads((root / "native/manifest.json").read_text())
    replacements = {
        "src/backend/lopdf_backend.rs": {"LOPDF_VERSION": versions["lopdf"]},
        "src/backend/docling_backend.rs": {"DOCLING_VERSION": versions["docling-pdf"]},
        "src/backend/pdfium_backend.rs": {
            "PDFIUM_RENDER_VERSION": versions["pdfium-render"],
            "PDFIUM_BINARY_VERSION": native["pdfium_release"],
        },
    }
    result = {}
    for filename, constants in replacements.items():
        path = root / filename
        original = path.read_text()
        text = original
        for constant, version in constants.items():
            text = replace_once(text, rf'(const {constant}: &str = ")[^"]+(";)', version)
        if filename.endswith("pdfium_backend.rs"):
            identity = f"{native['pdfium_release']}-binding-{versions['pdfium-render']}"
            text = replace_once(text, r'(assert_eq!\(identity.version, ")[^"]+("\);)', identity)
        if text != original:
            result[path] = text
    office = root / "src/ingest/office.rs"
    if office.exists():
        original = office.read_text()
        text = original
        for label, package in [
            ("docling-declarative", "docling"),
            ("csv", "csv"),
            ("calamine-sparse", "calamine"),
        ]:
            text = replace_once(
                text, rf'(identity\(\s*"{label}",\s*")[^"]+("\s*,)', versions[package]
            )
        if text != original:
            result[office] = text
    return result


def refresh(root: Path, *, check: bool = False) -> list[str]:
    changed = refreshed_files(root)
    if check and changed:
        names = ", ".join(str(path.relative_to(root)) for path in changed)
        raise ValueError(
            f"stale dependency identities: {names}; "
            "run uv run python scripts/refresh_dependency_versions.py"
        )
    for path, content in changed.items():
        path.write_text(content)
    return [str(path.relative_to(root)) for path in changed]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    for path in refresh(ROOT, check=args.check):
        print(path)


if __name__ == "__main__":
    main()
