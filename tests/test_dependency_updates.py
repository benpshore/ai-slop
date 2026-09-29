"""Offline maintenance-contract tests: no downloads, builds or PR publication."""

import hashlib
import io
import json
import sys
import tarfile
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import refresh_dependency_versions as identities  # noqa: E402
import update_dependencies as updates  # noqa: E402


@pytest.fixture
def repository(tmp_path):
    (tmp_path / "native").mkdir()
    (tmp_path / "src/backend").mkdir(parents=True)
    (tmp_path / "src/ingest").mkdir()
    (tmp_path / "Cargo.toml").write_text(
        '[package]\nname = "engine"\n[dependencies]\n'
        'docling-core = { version = "=1.0.0", optional = true }\n'
        'docling-pdf = { version = "=1.0.0", optional = true }\n'
        'docling-formats = { package = "docling", version = "=1.0.0" }\n'
    )
    locked = {
        "lopdf": "0.45.0",
        "docling-pdf": "1.0.0",
        "docling": "1.0.0",
        "pdfium-render": "0.8.37",
        "csv": "1.4.0",
        "calamine": "0.36.1",
    }
    dependencies = [name if name != "lopdf" else "lopdf 0.45.0" for name in locked]
    lock = '[[package]]\nname = "engine"\nversion = "0.1.0"\ndependencies = '
    lock += json.dumps(dependencies) + "\n"
    for name, version in [*locked.items(), ("lopdf", "0.44.0")]:
        lock += f'[[package]]\nname = "{name}"\nversion = "{version}"\n'
    (tmp_path / "Cargo.lock").write_text(lock)
    sources = {
        "lopdf_backend.rs": 'const LOPDF_VERSION: &str = "old";\n',
        "docling_backend.rs": 'const DOCLING_VERSION: &str = "old";\n',
        "pdfium_backend.rs": 'const PDFIUM_RENDER_VERSION: &str = "old";\n'
        'const PDFIUM_BINARY_VERSION: &str = "old";\n'
        'assert_eq!(identity.version, "old-binding-old");\n',
    }
    for filename, text in sources.items():
        (tmp_path / "src/backend" / filename).write_text(text)
    (tmp_path / "src/ingest/office.rs").write_text(
        'identity("docling-declarative", "old", policy);\n'
        'identity("csv", "old", policy);\n'
        'identity("calamine-sparse", "old", policy);\n'
    )
    manifest = {
        "version": 1,
        "pdfium_release": "chromium/1",
        "models_release": "models-v1",
        "entries": [
            {
                "kind": "pdfium",
                "platform": "linux-x64",
                "url": "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/1/pdfium-linux-x64.tgz",
                "member": "lib/libpdfium.so",
                "dest": ".pdfium/lib/libpdfium.so",
                "archive_sha256": "0" * 64,
                "sha256": "1" * 64,
            },
            {
                "kind": "model",
                "platform": "any",
                "url": "https://github.com/docling-project/docling.rs/releases/download/models-v1/ocr_rec_en.onnx",
                "member": None,
                "dest": ".models/ocr_rec_en.onnx",
                "archive_sha256": None,
                "sha256": "2" * 64,
            },
        ],
    }
    (tmp_path / "native/manifest.json").write_text(updates.format_manifest(manifest))
    return tmp_path


def archive_bytes(members):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz") as archive:
        for name, body, member_type in members:
            member = tarfile.TarInfo(name)
            member.type = member_type
            member.size = len(body)
            archive.addfile(member, io.BytesIO(body))
    return output.getvalue()


def upstream(repo, tag, name, content):
    return {
        "tag_name": tag,
        "html_url": f"https://github.com/{repo}/releases/tag/{tag}",
        "published_at": "2026-09-29T00:00:00Z",
        "assets": [
            {
                "name": name,
                "size": len(content),
                "digest": "sha256:" + hashlib.sha256(content).hexdigest(),
                "browser_download_url": f"https://github.com/{repo}/releases/download/{tag}/{name}",
            }
        ],
    }


def test_versions_follow_direct_root_resolution(repository):
    assert identities.root_versions(repository)["lopdf"] == "0.45.0"
    identities.refresh(repository)
    text = (repository / "src/backend/pdfium_backend.rs").read_text()
    assert '"chromium/1-binding-0.8.37"' in text
    assert identities.refresh(repository, check=True) == []


def test_stale_identities_fail_without_modifying_files(repository):
    path = repository / "src/backend/lopdf_backend.rs"
    original = path.read_bytes()
    with pytest.raises(ValueError, match="stale dependency identities"):
        identities.refresh(repository, check=True)
    assert path.read_bytes() == original


def test_missing_identity_fails_instead_of_silently_skipping(repository):
    (repository / "src/backend/lopdf_backend.rs").write_text("missing")
    with pytest.raises(ValueError, match="expected one version"):
        identities.refresh(repository)


def test_pdfium_pins_both_archive_and_regular_library(repository, monkeypatch):
    payload = b"a new library"
    archive = archive_bytes([("lib/libpdfium.so", payload, tarfile.REGTYPE)])
    released = upstream(updates.PDFIUM_REPO, "chromium/2", "pdfium-linux-x64.tgz", archive)
    monkeypatch.setattr(updates, "release", lambda *_args: released)
    monkeypatch.setattr(updates, "fetch", lambda *_args: archive)
    assert updates.native_update(repository, "pdfium")
    path = repository / "native/manifest.json"
    manifest = json.loads(path.read_text())
    entry = manifest["entries"][0]
    assert manifest["pdfium_release"] == "chromium/2"
    assert entry["sha256"] == hashlib.sha256(payload).hexdigest()
    assert entry["archive_sha256"] == hashlib.sha256(archive).hexdigest()
    assert manifest["entries"][1]["sha256"] == "2" * 64
    assert len([line for line in path.read_text().splitlines() if '"kind"' in line]) == 2


def test_unchanged_release_does_not_download_assets(repository, monkeypatch):
    content = b"same archive"
    released = upstream(updates.PDFIUM_REPO, "chromium/1", "pdfium-linux-x64.tgz", content)
    released["assets"][0]["digest"] = "sha256:" + "0" * 64
    monkeypatch.setattr(updates, "release", lambda *_args: released)
    monkeypatch.setattr(updates, "fetch", lambda *_args: pytest.fail("unexpected download"))
    assert updates.native_update(repository, "pdfium") == []


def test_bad_download_does_not_repin_manifest(repository, monkeypatch):
    released = upstream(updates.PDFIUM_REPO, "chromium/2", "pdfium-linux-x64.tgz", b"expected")
    monkeypatch.setattr(updates, "release", lambda *_args: released)
    monkeypatch.setattr(updates, "fetch", lambda *_args: b"incorrect")
    path = repository / "native/manifest.json"
    original = path.read_bytes()
    with pytest.raises(ValueError, match="checksum mismatch"):
        updates.native_update(repository, "pdfium")
    assert path.read_bytes() == original


@pytest.mark.parametrize("kind", [tarfile.SYMTYPE, tarfile.LNKTYPE, tarfile.DIRTYPE])
def test_library_must_be_regular_file(kind):
    archive = archive_bytes([("lib/libpdfium.so", b"", kind)])
    with pytest.raises(ValueError, match="exactly one regular"):
        updates.library_digest(archive, "lib/libpdfium.so")


def test_duplicate_library_is_rejected():
    member = ("lib/libpdfium.so", b"library", tarfile.REGTYPE)
    with pytest.raises(ValueError, match="exactly one regular"):
        updates.library_digest(archive_bytes([member, member]), member[0])


def test_archive_expansion_is_bounded(monkeypatch):
    archive = archive_bytes([("unrelated", b"big", tarfile.REGTYPE)])
    monkeypatch.setattr(updates, "MAX_EXPANDED", 2)
    with pytest.raises(ValueError, match="expansion"):
        updates.library_digest(archive, "lib/libpdfium.so")


@pytest.mark.parametrize(
    "field,value", [("digest", None), ("size", 0), ("size", updates.MAX_ASSET + 1)]
)
def test_asset_requires_known_digest_and_size(field, value):
    released = upstream(updates.PDFIUM_REPO, "chromium/2", "a.tgz", b"bytes")
    released["assets"][0][field] = value
    with pytest.raises(ValueError, match="SHA-256 digest"):
        updates.asset(released, "a.tgz", updates.PDFIUM_REPO)


def test_asset_url_must_belong_to_expected_release():
    released = upstream(updates.PDFIUM_REPO, "chromium/2", "a.tgz", b"bytes")
    released["assets"][0]["browser_download_url"] = "https://other.example/a.tgz"
    with pytest.raises(ValueError, match="unexpected asset URL"):
        updates.asset(released, "a.tgz", updates.PDFIUM_REPO)


def test_docling_selects_common_stable_unyanked_version(repository, monkeypatch):
    def versions(url):
        return {
            "versions": [
                {"num": "2.0.0", "yanked": url.endswith("docling-core")},
                {"num": "1.2.0", "yanked": False},
                {"num": "1.3.0-rc.1", "yanked": False},
            ]
        }

    monkeypatch.setattr(updates, "api_json", versions)
    monkeypatch.setattr(
        updates,
        "release",
        lambda repo, tag: {"html_url": f"https://github.com/{repo}/releases/tag/{tag}"},
    )
    commands = []
    monkeypatch.setattr(
        updates.subprocess, "run", lambda command, **_kwargs: commands.append(command)
    )
    monkeypatch.setattr(updates, "refresh", lambda _root: [])
    report = updates.docling_update(repository)
    assert "1.2.0" in report[0]
    assert "=1.2.0" in (repository / "Cargo.toml").read_text()
    assert commands == [
        [
            "cargo",
            "update",
            "-p",
            "docling",
            "-p",
            "docling-core",
            "-p",
            "docling-pdf",
            "-p",
            "docling-onnx",
            "-p",
            "docling-asr",
        ]
    ]


def test_current_repository_identities_match_lock():
    assert identities.refresh(ROOT, check=True) == []


def test_model_replacement_keeps_separate_release_track(repository, monkeypatch):
    payload = b"updated OCR model"
    released = upstream(updates.DOCLING_REPO, "models-v1", "ocr_rec_en.onnx", payload)
    requests = []

    def release(repo, tag):
        requests.append((repo, tag))
        return released

    monkeypatch.setattr(updates, "release", release)
    monkeypatch.setattr(updates, "fetch", lambda *_args: payload)
    assert updates.native_update(repository, "models")
    manifest = json.loads((repository / "native/manifest.json").read_text())
    assert requests == [(updates.DOCLING_REPO, "models-v1")]
    assert manifest["models_release"] == "models-v1"
    assert manifest["pdfium_release"] == "chromium/1"
    assert manifest["entries"][1]["sha256"] == hashlib.sha256(payload).hexdigest()


def test_release_rejects_prerelease_or_wrong_tag(monkeypatch):
    monkeypatch.setattr(updates, "api_json", lambda _url: {"tag_name": "npm-cuda-v1.2.0"})
    with pytest.raises(ValueError, match="tag mismatch"):
        updates.release(updates.DOCLING_REPO, "v1.2.0")
    monkeypatch.setattr(
        updates, "api_json", lambda _url: {"tag_name": "v1.2.0", "prerelease": True}
    )
    with pytest.raises(ValueError, match="stable"):
        updates.release(updates.DOCLING_REPO, "v1.2.0")
