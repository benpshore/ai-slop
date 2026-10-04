"""Safety and round-trip tests for the repository's staged Site source exchange."""
# Ruff's repository test-path exception covers tests/, not this web-only suite.
# ruff: noqa: S101

from __future__ import annotations

import importlib.util
import json
import os
import stat
from pathlib import Path
from unittest import mock

import pytest

SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "site_source.py"
SPEC = importlib.util.spec_from_file_location("site_source", SCRIPT)
assert SPEC and SPEC.loader
site_source = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(site_source)


@pytest.fixture
def source(tmp_path):
    root = tmp_path / "site"
    root.mkdir()
    for name in site_source.REQUIRED:
        (root / name).write_text("{}\n", encoding="utf-8")
    (root / ".gitignore").write_text("/node_modules/\n.env*\n", encoding="utf-8")
    (root / "app").mkdir()
    (root / "app" / "page.tsx").write_text("export default 'original';\n", encoding="utf-8")
    return root


def export(source, output):
    assert site_source.main(["export", "--source", str(source), "--output", str(output)]) == 0
    return output


def tree_bytes(root):
    return {p.relative_to(root).as_posix(): p.read_bytes() for p in root.rglob("*") if p.is_file()}


def rewrite_manifest(bundle, edit):
    path = bundle / site_source.MANIFEST
    manifest = json.loads(path.read_text())
    edit(manifest)
    path.write_text(json.dumps(manifest))


def test_deterministic_export_and_verified_import_preserve_all_source_bytes(source, tmp_path):
    first = export(source, tmp_path / "first")
    second = export(source, tmp_path / "second")
    assert tree_bytes(first) == tree_bytes(second)
    assert (first / "app/page.tsx").read_bytes() == (source / "app/page.tsx").read_bytes()
    assert (first / ".gitignore").read_bytes().endswith(site_source.GITIGNORE_APPENDIX)
    stage = tmp_path / "stage"
    assert site_source.main(["import", "--source", str(first), "--output", str(stage)]) == 0
    assert tree_bytes(first) == tree_bytes(stage)
    assert site_source.main(["verify", "--source", str(stage)]) == 0
    third = export(first, tmp_path / "third")
    assert tree_bytes(first) == tree_bytes(third)


def test_runtime_secrets_databases_and_generated_wasm_are_never_exported(source, tmp_path):
    excluded = [
        ".openai/hosting.json",
        ".env",
        ".env.production",
        ".env.example",
        ".npmrc",
        ".git/config",
        ".sites-runtime/state",
        ".wrangler/local.sqlite",
        "node_modules/x.js",
        "dist/worker.js",
        "app/.env.local",
        "app/nested/.openai/hosting.json",
        "lib/credentials.json",
        "lib/secrets.yaml",
        "lib/private.key",
        "lib/local.sqlite",
        "public/vendor/pdf-oxide/pdf_oxide.js",
        "public/vendor/pdf-oxide/pdf_oxide_bg.wasm",
        "public/ocr/7.0.0/worker.min.js",
        "public/node_modules/nested.js",
        "tsconfig.tsbuildinfo",
        "next-env.d.ts",
    ]
    for name in excluded:
        path = source / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("DO_NOT_COPY")
    for name in [
        "build/sites-worker.ts",
        "hooks/use-mobile.ts",
        "drizzle/0000.sql",
        "vendor/style.css",
    ]:
        path = source / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("source")
    bundle = export(source, tmp_path / "bundle")
    assert all(not (bundle / name).exists() for name in excluded)
    assert all(b"DO_NOT_COPY" not in data for data in tree_bytes(bundle).values())
    assert (bundle / "build/sites-worker.ts").read_text() == "source"


@pytest.mark.parametrize("existing", ["directory", "file", "symlink"])
def test_existing_output_is_never_replaced(source, tmp_path, existing):
    target = tmp_path / "existing"
    if existing == "directory":
        target.mkdir()
        (target / ".env").write_text("private")
    elif existing == "file":
        target.write_text("private")
    else:
        actual = tmp_path / "actual"
        actual.mkdir()
        (actual / ".env").write_text("private")
        target.symlink_to(actual, target_is_directory=True)
    before = tree_bytes(tmp_path)
    assert site_source.main(["export", "--source", str(source), "--output", str(target)]) == 1
    assert tree_bytes(tmp_path) == before


def test_symlinked_output_ancestor_is_refused(source, tmp_path):
    actual = tmp_path / "actual"
    actual.mkdir()
    link = tmp_path / "link"
    link.symlink_to(actual, target_is_directory=True)
    assert site_source.main(["export", "--source", str(source), "--output", str(link / "out")]) == 1
    assert not (actual / "out").exists()


@pytest.mark.parametrize("directory", [False, True])
def test_symlinked_source_never_reads_outside_the_checkout(source, tmp_path, directory):
    external = tmp_path / "external"
    if directory:
        external.mkdir()
        (external / "secret.ts").write_text("private")
    else:
        external.write_text("private")
    (source / "app" / "linked").symlink_to(external, target_is_directory=directory)
    output = tmp_path / "bundle"
    assert site_source.main(["export", "--source", str(source), "--output", str(output)]) == 1
    assert not output.exists()


def test_normalized_output_cannot_be_nested_inside_source(source):
    output = source.parent / "other" / ".." / source.name / "bundle"
    assert site_source.main(["export", "--source", str(source), "--output", str(output)]) == 1
    assert not (source / "bundle").exists()


@pytest.mark.parametrize("mutation", ["bytes", "missing", "unlisted", "mode"])
def test_tampering_fails_before_creating_import_stage(source, tmp_path, mutation):
    bundle = export(source, tmp_path / "bundle")
    page = bundle / "app/page.tsx"
    if mutation == "bytes":
        page.write_text("tampered")
    elif mutation == "missing":
        page.unlink()
    elif mutation == "unlisted":
        (bundle / "app/unlisted.ts").write_text("extra")
    else:
        page.chmod(0o755)
    output = tmp_path / "stage"
    assert site_source.main(["import", "--source", str(bundle), "--output", str(output)]) == 1
    assert not output.exists()


@pytest.mark.parametrize(
    "path",
    [
        "../escape",
        "/absolute",
        "app/../../escape",
        "app//file",
        "app\\file",
        "app/.env",
        ".openai/hosting.json",
    ],
)
def test_unsafe_manifest_paths_are_refused(source, tmp_path, path):
    bundle = export(source, tmp_path / "bundle")
    rewrite_manifest(bundle, lambda value: value["files"][0].update(path=path))
    with pytest.raises(site_source.SourceError):
        site_source.verify(bundle)


def test_duplicate_manifest_entries_and_fields_are_rejected(source, tmp_path):
    bundle = export(source, tmp_path / "bundle")
    rewrite_manifest(bundle, lambda value: value["files"].append(value["files"][0]))
    with pytest.raises(site_source.SourceError, match="duplicate"):
        site_source.verify(bundle)
    (bundle / site_source.MANIFEST).write_text('{"format":"x","format":"y","files":[]}')
    with pytest.raises(site_source.SourceError, match="Duplicate"):
        site_source.verify(bundle)


def test_import_comparison_is_read_only_and_retains_target_only_files(source, tmp_path):
    bundle = export(source, tmp_path / "bundle")
    (source / "app/page.tsx").write_text("local change")
    (source / "app/local.ts").write_text("retain this")
    (source / ".env").write_text("private credentials")
    (source / ".openai").mkdir()
    (source / ".openai/hosting.json").write_text("private deployment configuration")
    before = tree_bytes(source)
    stage = tmp_path / "stage"
    assert (
        site_source.main(
            ["import", "--source", str(bundle), "--output", str(stage), "--against", str(source)]
        )
        == 0
    )
    assert tree_bytes(source) == before
    plan = json.loads((stage / "IMPORT_PLAN.json").read_text())
    assert plan["modify"] == ["app/page.tsx"]
    assert plan["only_in_target_keep"] == ["app/local.ts"]
    assert not (stage / ".env").exists()
    assert not (stage / ".openai").exists()
    site_source.verify(stage)


def test_import_does_not_copy_ignored_extra_files_from_bundle(source, tmp_path):
    bundle = export(source, tmp_path / "bundle")
    (bundle / ".env").write_text("private")
    (bundle / "tests").mkdir()
    (bundle / "tests/ignored.py").write_text("mirror tooling, not app source")
    stage = tmp_path / "stage"
    assert site_source.main(["import", "--source", str(bundle), "--output", str(stage)]) == 0
    assert not (stage / ".env").exists()
    assert not (stage / "tests").exists()


def test_executable_bit_preserved_without_setuid(source, tmp_path):
    script = source / "app/tool.sh"
    script.write_text("#!/bin/sh\nexit 0\n")
    script.chmod(0o4755)
    bundle = export(source, tmp_path / "bundle")
    assert stat.S_IMODE((bundle / "app/tool.sh").stat().st_mode) == 0o755


def test_private_key_material_and_missing_lockfile_fail_export(source, tmp_path):
    (source / "app/key.ts").write_text("-----BEGIN PRIVATE KEY-----\nexample\n")
    assert (
        site_source.main(["export", "--source", str(source), "--output", str(tmp_path / "one")])
        == 1
    )
    (source / "app/key.ts").unlink()
    (source / "pnpm-lock.yaml").unlink()
    assert (
        site_source.main(["export", "--source", str(source), "--output", str(tmp_path / "two")])
        == 1
    )
    assert not (tmp_path / "one").exists() and not (tmp_path / "two").exists()


def test_changing_source_is_not_published(source, tmp_path):
    files = site_source.source_files(source, export=True)
    changed = {**files, "app/other.ts": (b"new", "0644")}
    with mock.patch.object(site_source, "source_files", side_effect=[files, changed]):
        assert (
            site_source.main(["export", "--source", str(source), "--output", str(tmp_path / "out")])
            == 1
        )
    assert not (tmp_path / "out").exists()


def test_failed_stage_verification_cleans_only_its_new_directory(source, tmp_path):
    files = site_source.source_files(source, export=True)
    marker = tmp_path / "retain"
    marker.write_text("retain")
    with (
        mock.patch.object(site_source, "verify", side_effect=site_source.SourceError("failure")),
        pytest.raises(site_source.SourceError),
    ):
        site_source.write_bundle(tmp_path / "out", files)
    assert not (tmp_path / "out").exists()
    assert marker.read_text() == "retain"


@pytest.mark.skipif(not hasattr(os, "mkfifo"), reason="named pipes require POSIX")
def test_nonregular_source_is_rejected_without_blocking(source, tmp_path):
    os.mkfifo(source / "app/pipe")
    assert (
        site_source.main(["export", "--source", str(source), "--output", str(tmp_path / "out")])
        == 1
    )
