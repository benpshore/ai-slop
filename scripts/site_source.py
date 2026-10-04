#!/usr/bin/env python3
"""Export, verify, and stage the web alpha's portable source (never deploy it)."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import stat
import sys
from pathlib import Path, PurePosixPath

FORMAT = "tpe-site-source-v1"
MANIFEST = "SOURCE_MANIFEST.json"
ROOTS = frozenset(
    {
        "app",
        "components",
        "lib",
        "db",
        "drizzle",
        "public",
        "scripts",
        "vendor",
        "build",
        "hooks",
        "docs",
    }
)
CONFIGS = frozenset(
    {
        ".gitignore",
        "README.md",
        "package.json",
        "pnpm-lock.yaml",
        "pnpm-workspace.yaml",
        "tsconfig.json",
        "vite.config.ts",
        "next.config.ts",
        "drizzle.config.ts",
        "eslint.config.mjs",
        "postcss.config.mjs",
        "components.json",
        "cloudflare-env.d.ts",
    }
)
REQUIRED = {"package.json", "pnpm-lock.yaml", "tsconfig.json", "vite.config.ts"}
EXCLUDED_NAMES = frozenset(
    {
        "node_modules",
        "dist",
        "target",
        "coverage",
        "__pycache__",
        "credentials.json",
        "secrets.json",
        "secrets.yaml",
        "secrets.yml",
    }
)
EXCLUDED_SUFFIXES = (
    ".pem",
    ".key",
    ".p12",
    ".pfx",
    ".db",
    ".db-journal",
    ".db-wal",
    ".db-shm",
    ".sqlite",
    ".sqlite3",
    ".sqlite-journal",
    ".sqlite-wal",
    ".sqlite-shm",
    ".log",
    ".pyc",
    ".tsbuildinfo",
    ".bak",
    ".swp",
    ".swo",
)
MAX_FILES = 10_000
MAX_FILE_BYTES = 16 * 1024 * 1024
MAX_TOTAL_BYTES = 64 * 1024 * 1024
GITIGNORE_APPENDIX = b"""
# Portable source mirror: protect local deployment state and generated assets.
/.openai/
/public/vendor/pdf-oxide/
/public/ocr/
*.tsbuildinfo
__pycache__/
/IMPORT_PLAN.json
# The parent Rust repository ignores build/; these are web build source files.
!/build/
!/build/**
"""


class SourceError(ValueError):
    """A source bundle is incomplete, unsafe, or has changed."""


def allowed_path(value: str) -> bool:
    """Only canonical, relative, allowlisted source paths can be materialized."""
    if not value or "\\" in value or "\x00" in value:
        return False
    path = PurePosixPath(value)
    if path.is_absolute() or str(path) != value or any(p in {".", ".."} for p in path.parts):
        return False
    if path.parts[0] not in ROOTS and value not in CONFIGS:
        return False
    if path.parts[:3] == ("public", "vendor", "pdf-oxide") or path.parts[:2] == (
        "public",
        "ocr",
    ):
        return False
    for part in path.parts:
        lower = part.lower()
        if (part.startswith(".") and value != ".gitignore") or lower in EXCLUDED_NAMES:
            return False
        if lower.startswith(("secrets.", "credentials.")) or lower.endswith(EXCLUDED_SUFFIXES):
            return False
    return True


def no_symlink_ancestors(path: Path) -> None:
    for candidate in (path, *path.parents):
        if candidate.is_symlink():
            raise SourceError(f"Symlinks are not accepted: {candidate}")


def read_regular(path: Path) -> tuple[bytes, str]:
    no_symlink_ancestors(path)
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
    with os.fdopen(os.open(path, flags), "rb") as source:
        info = os.fstat(source.fileno())
        if not stat.S_ISREG(info.st_mode):
            raise SourceError(f"Only regular source files are accepted: {path}")
        if info.st_size > MAX_FILE_BYTES:
            raise SourceError(f"Source file exceeds {MAX_FILE_BYTES} bytes: {path}")
        data = source.read(MAX_FILE_BYTES + 1)
    if len(data) > MAX_FILE_BYTES:
        raise SourceError(f"Source file grew beyond its limit: {path}")
    # Permission normalization retains executable scripts, never privileged bits.
    return data, "0755" if info.st_mode & 0o111 else "0644"


def source_files(root: Path, *, export: bool = False) -> dict[str, tuple[bytes, str]]:
    no_symlink_ancestors(root)
    if not root.is_dir():
        raise SourceError(f"Source directory does not exist: {root}")
    files: dict[str, tuple[bytes, str]] = {}
    total = 0

    def visit(path: Path) -> None:
        nonlocal total
        relative = path.relative_to(root).as_posix()
        if not allowed_path(relative):
            return
        if path.is_symlink():
            raise SourceError(f"Source symlink is not accepted: {relative}")
        if path.is_dir():
            for child in sorted(path.iterdir()):
                visit(child)
            return
        data, mode = read_regular(path)
        if b"-----BEGIN " in data and b"PRIVATE KEY-----" in data:
            raise SourceError(f"Private key material is not accepted: {relative}")
        if export and relative == ".gitignore" and not data.endswith(GITIGNORE_APPENDIX):
            data = data.rstrip(b"\n") + b"\n" + GITIGNORE_APPENDIX
        total += len(data)
        if len(files) >= MAX_FILES or total > MAX_TOTAL_BYTES:
            raise SourceError("Source bundle exceeds its file-count or byte limit")
        files[relative] = (data, mode)

    for child in sorted(root.iterdir()):
        visit(child)
    missing = REQUIRED - files.keys()
    if missing:
        raise SourceError(f"Required source files missing: {', '.join(sorted(missing))}")
    if export and ".gitignore" not in files:
        files[".gitignore"] = (GITIGNORE_APPENDIX, "0644")
    return files


def manifest_bytes(files: dict[str, tuple[bytes, str]]) -> bytes:
    entries = [
        {"path": path, "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data), "mode": mode}
        for path, (data, mode) in sorted(files.items())
    ]
    return (
        json.dumps({"format": FORMAT, "files": entries}, indent=2, sort_keys=True) + "\n"
    ).encode()


def unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise SourceError(f"Duplicate manifest field: {key}")
        result[key] = value
    return result


def verify(root: Path) -> dict[str, tuple[bytes, str]]:
    raw, _ = read_regular(root / MANIFEST)
    try:
        manifest = json.loads(raw, object_pairs_hook=unique_object)
    except (UnicodeError, json.JSONDecodeError) as error:
        raise SourceError("Source manifest is not valid UTF-8 JSON") from error
    if not isinstance(manifest, dict) or set(manifest) != {"format", "files"}:
        raise SourceError("Invalid source manifest structure")
    if manifest["format"] != FORMAT or not isinstance(manifest["files"], list):
        raise SourceError("Unsupported source manifest format")
    if len(manifest["files"]) > MAX_FILES:
        raise SourceError("Too many manifest entries")
    paths = set()
    for entry in manifest["files"]:
        if not isinstance(entry, dict) or set(entry) != {"path", "sha256", "bytes", "mode"}:
            raise SourceError("Invalid source manifest entry")
        path = entry["path"]
        if not isinstance(path, str) or not allowed_path(path) or path in paths:
            raise SourceError(f"Unsafe or duplicate manifest path: {path!r}")
        if (
            not isinstance(entry["mode"], str)
            or entry["mode"] not in {"0644", "0755"}
            or type(entry["bytes"]) is not int
        ):
            raise SourceError(f"Invalid source mode or size: {path}")
        paths.add(path)
    files = source_files(root)
    expected = json.loads(manifest_bytes(files))
    if manifest != expected:
        raise SourceError(
            "Manifest verification failed: source paths, bytes, modes, or SHA-256 changed"
        )
    return files


def destination_path(source: Path, output: Path) -> Path:
    output = Path(os.path.abspath(output))
    no_symlink_ancestors(output)
    if output.exists():
        raise SourceError(f"Output already exists; choose a new staging directory: {output}")
    if not output.parent.is_dir():
        raise SourceError(f"Output parent must already exist: {output.parent}")
    if output.is_relative_to(source.resolve()):
        raise SourceError("Output must be outside the source tree")
    return output


def write_bundle(
    output: Path, files: dict[str, tuple[bytes, str]], plan: dict | None = None
) -> None:
    # Exclusive mkdir prevents overwriting a checkout, credentials, or another stage.
    output.mkdir(mode=0o755, exist_ok=False)
    try:
        for path, (data, mode) in sorted(files.items()):
            target = output / path
            target.parent.mkdir(parents=True, exist_ok=True)
            with target.open("xb") as handle:
                handle.write(data)
            target.chmod(int(mode, 8))
        with (output / MANIFEST).open("xb") as handle:
            handle.write(manifest_bytes(files))
        if plan is not None:
            with (output / "IMPORT_PLAN.json").open("x", encoding="utf-8") as handle:
                json.dump(plan, handle, indent=2, sort_keys=True)
                handle.write("\n")
        verify(output)
    except BaseException:
        # This directory was exclusively created by this invocation.
        shutil.rmtree(output)
        raise


def compare(files: dict[str, tuple[bytes, str]], target: Path) -> dict[str, list[str]]:
    previous = source_files(target, export=True)
    return {
        "add": sorted(files.keys() - previous.keys()),
        "modify": sorted(
            path for path in files.keys() & previous.keys() if files[path] != previous[path]
        ),
        "unchanged": sorted(
            path for path in files.keys() & previous.keys() if files[path] == previous[path]
        ),
        "only_in_target_keep": sorted(previous.keys() - files.keys()),
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("export", "verify", "import"):
        command = commands.add_parser(name)
        command.add_argument("--source", type=Path, required=True)
        if name != "verify":
            command.add_argument(
                "--output", type=Path, required=True, help="new, nonexistent staging directory"
            )
        if name == "import":
            command.add_argument(
                "--against", type=Path, help="read-only comparison with an existing Site checkout"
            )
    args = parser.parse_args(argv)
    try:
        source = Path(os.path.abspath(args.source))
        if args.command == "export":
            output = destination_path(source, args.output)
            files = source_files(source, export=True)
            # Refuse a mixed snapshot when the live source changed during capture.
            if source_files(source, export=True) != files:
                raise SourceError("Source changed while being exported; retry after edits settle")
            write_bundle(output, files)
        else:
            files = verify(source)
            if args.command == "import":
                output = destination_path(source, args.output)
                plan = compare(files, args.against.absolute()) if args.against else None
                write_bundle(output, files, plan)
        print(
            json.dumps(
                {"command": args.command, "files": len(files), "verified": True}, sort_keys=True
            )
        )
        return 0
    except (OSError, SourceError) as error:
        print(f"site_source: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
