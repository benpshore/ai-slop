"""Prepare a bounded, reviewable Docling/PDFium/model dependency update.

Run through uv. Runtime provisioning continues to use native/manifest.json;
this maintenance tool alone resolves current releases. It never pushes or merges.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import re
import subprocess
import tarfile
import tomllib
import urllib.parse
import urllib.request
from pathlib import Path

from refresh_dependency_versions import ROOT, refresh

PDFIUM_REPO = "bblanchon/pdfium-binaries"
DOCLING_REPO = "docling-project/docling.rs"
MAX_JSON = 4 * 1024 * 1024
MAX_ASSET = 128 * 1024 * 1024
MAX_MEMBER = 128 * 1024 * 1024
MAX_EXPANDED = 256 * 1024 * 1024
VERSION = re.compile(r"^\d+\.\d+\.\d+$")
DIGEST = re.compile(r"^sha256:([0-9a-f]{64})$")


def fetch(url: str, limit: int, *, api: bool = False) -> bytes:
    parsed = urllib.parse.urlsplit(url)
    allowed = {"api.github.com", "crates.io"} if api else {"github.com"}
    if parsed.scheme != "https" or parsed.hostname not in allowed:
        raise ValueError(f"unexpected update URL: {url}")
    headers = {"User-Agent": "pdftextract-dependency-maintenance"}
    if parsed.hostname == "api.github.com":
        headers["Accept"] = "application/vnd.github+json"
        if token := os.environ.get("GH_TOKEN"):
            headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, headers=headers)  # noqa: S310 - HTTPS allowlist above
    with urllib.request.urlopen(request, timeout=30) as response:  # noqa: S310 - HTTPS allowlist above
        content = response.read(limit + 1)
    if len(content) > limit:
        raise ValueError(f"download exceeds {limit} bytes: {url}")
    return content


def api_json(url: str) -> dict:
    return json.loads(fetch(url, MAX_JSON, api=True))


def release(repo: str, tag: str | None = None) -> dict:
    suffix = "latest" if tag is None else f"tags/{urllib.parse.quote(tag, safe='')}"
    result = api_json(f"https://api.github.com/repos/{repo}/releases/{suffix}")
    if result.get("draft") or result.get("prerelease") or not result.get("tag_name"):
        raise ValueError("only published stable releases can be proposed")
    if tag is not None and result["tag_name"] != tag:
        raise ValueError("release tag mismatch")
    return result


def asset(released: dict, name: str, repo: str) -> tuple[str, str, int]:
    matches = [item for item in released["assets"] if item["name"] == name]
    if len(matches) != 1:
        raise ValueError(f"expected one release asset {name}")
    item = matches[0]
    digest = DIGEST.fullmatch(item.get("digest") or "")
    size = item.get("size")
    if not digest or not isinstance(size, int) or not 0 < size <= MAX_ASSET:
        raise ValueError(f"asset {name} needs a SHA-256 digest and bounded positive size")
    tag = released["tag_name"]
    expected = f"https://github.com/{repo}/releases/download/{tag}/{name}"
    if urllib.parse.unquote(item["browser_download_url"]) != expected:
        raise ValueError(f"unexpected asset URL for {name}")
    return item["browser_download_url"], digest[1], size


def downloaded(url: str, digest: str, size: int) -> bytes:
    content = fetch(url, min(size, MAX_ASSET))
    if len(content) != size or hashlib.sha256(content).hexdigest() != digest:
        raise ValueError(f"asset size/checksum mismatch: {url}")
    return content


def library_digest(content: bytes, member_name: str) -> str:
    # Read only the selected regular file. Never extract paths, links or owners.
    with tarfile.open(fileobj=io.BytesIO(content), mode="r:gz") as archive:
        matches = []
        expanded = 0
        for index, member in enumerate(archive):
            expanded += member.size
            if index >= 10_000 or expanded > MAX_EXPANDED:
                raise ValueError("archive exceeds expansion/member-count limit")
            if member.name == member_name:
                matches.append(member)
        if len(matches) != 1 or not matches[0].isfile():
            raise ValueError(f"expected exactly one regular archive member: {member_name}")
        member = matches[0]
        if not 0 < member.size <= MAX_MEMBER:
            raise ValueError("library exceeds the member size limit")
        stream = archive.extractfile(member)
        if stream is None:
            raise ValueError("library member is unreadable")
        data = stream.read(MAX_MEMBER + 1)
        if len(data) != member.size:
            raise ValueError("truncated or oversized library member")
        return hashlib.sha256(data).hexdigest()


def format_manifest(manifest: dict) -> str:
    # Preserve native/fetch.sh's Python-free, one-entry-per-line fallback.
    entries = manifest["entries"]
    fields = [
        f"  {json.dumps(key)}: {json.dumps(value)},"
        for key, value in manifest.items()
        if key != "entries"
    ]
    rows = ["    " + json.dumps(entry, ensure_ascii=True) for entry in entries]
    return "{\n" + "\n".join(fields) + '\n  "entries": [\n' + ",\n".join(rows) + "\n  ]\n}\n"


def native_update(root: Path, track: str) -> list[str]:
    path = root / "native/manifest.json"
    manifest = json.loads(path.read_text())
    repo = PDFIUM_REPO if track == "pdfium" else DOCLING_REPO
    # Models are a separate asset track, never an npm/crate release number.
    released = release(repo, None if track == "pdfium" else manifest["models_release"])
    if track == "pdfium" and not re.fullmatch(r"chromium/\d+", released["tag_name"]):
        raise ValueError("PDFium latest release is not a chromium build")
    changes = []
    for entry in manifest["entries"]:
        if entry["kind"] != ("pdfium" if track == "pdfium" else "model"):
            continue
        name = f"pdfium-{entry['platform']}.tgz" if track == "pdfium" else Path(entry["dest"]).name
        url, checksum, size = asset(released, name, repo)
        old = entry["archive_sha256"] if track == "pdfium" else entry["sha256"]
        same_url = urllib.parse.unquote(entry["url"]) == urllib.parse.unquote(url)
        if old == checksum and same_url:
            continue
        content = downloaded(url, checksum, size)
        if track == "pdfium":
            entry["archive_sha256"] = checksum
            entry["sha256"] = library_digest(content, entry["member"])
        else:
            entry["sha256"] = checksum
        entry["url"] = url
        changes.append(
            f"- `{name}` ({size} bytes): `{old}` → `{checksum}`; "
            f"installed-file SHA-256 `{entry['sha256']}`."
        )
    if track == "pdfium":
        manifest["pdfium_release"] = released["tag_name"]
    if changes:
        path.write_text(format_manifest(manifest))
        refresh(root)
        changes.insert(
            0, f"Upstream release: {released['html_url']} (published {released['published_at']}).\n"
        )
    return changes


def docling_update(root: Path) -> list[str]:
    names = ["docling", "docling-core", "docling-pdf", "docling-onnx", "docling-asr"]
    versions = []
    for name in names:
        result = api_json(f"https://crates.io/api/v1/crates/{name}")
        versions.append(
            {
                item["num"]
                for item in result["versions"]
                if VERSION.fullmatch(item["num"]) and not item["yanked"]
            }
        )
    common = set.intersection(*versions)
    if not common:
        raise ValueError("no common stable Docling suite version")
    latest = max(common, key=lambda version: tuple(map(int, version.split("."))))
    upstream = release(DOCLING_REPO, f"v{latest}")
    path = root / "Cargo.toml"
    original = path.read_text()
    manifest = tomllib.loads(original)
    current = manifest["dependencies"]["docling-pdf"]["version"].removeprefix("=")
    if tuple(map(int, current.split("."))) >= tuple(map(int, latest.split("."))):
        return []
    text = original
    for alias in ["docling-core", "docling-pdf", "docling-formats"]:
        pattern = rf'(?m)^({alias} = \{{[^\n]*?version = ")[^"]+("[^\n]*\}})$'
        text, count = re.subn(pattern, lambda match: match[1] + "=" + latest + match[2], text)
        if count != 1:
            raise ValueError(f"missing unique manifest dependency {alias}")
    path.write_text(text)
    # Only named packages are updated. No global `cargo update` surprise.
    command = ["cargo", "update"]
    for name in names:
        command.extend(["-p", name])
    subprocess.run(command, cwd=root, check=True, timeout=180)  # noqa: S603 - fixed Cargo command, no shell
    refresh(root)
    return [
        f"Docling suite `{current}` → `{latest}`.\n",
        f"Upstream release: {upstream['html_url']}.",
        "",
        "The lockfile and extractor identity constants are updated together. "
        "Inspect upstream behavior and documentation; passing fixtures do not establish "
        "corpus accuracy.",
    ]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--track", required=True, choices=["docling", "pdfium", "models"])
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    changes = docling_update(ROOT) if args.track == "docling" else native_update(ROOT, args.track)
    body = "\n".join(changes) if changes else "No dependency changes."
    body += (
        "\n\nUpdates remain subject to CI and review; no automatic merge is enabled. "
        "Corpus benchmarks are not requested.\n"
    )
    if args.report:
        args.report.write_text(body)
    print(body)


if __name__ == "__main__":
    main()
