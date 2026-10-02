import os
import shutil
import sys
import subprocess
from pathlib import Path

import pytest

import pdftextract_engine as engine

FIXTURE = Path(__file__).parent / "fixtures" / "layout.pdf"


def paper(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(FIXTURE, path)
    return path


def test_core_stays_under_100_lines():
    assert len(Path(engine.__file__).read_text().splitlines()) <= 100


def test_real_poppler_preserves_layout(tmp_path):
    source = paper(tmp_path / "paper with spaces.PDF")
    target = engine.convert_pdf(source)
    expected = subprocess.check_output(["pdftotext", "-layout", "-enc", "UTF-8", str(source), "-"])
    assert target.read_bytes() == expected
    assert b"Left column" in expected and b"Right column" in expected
    first = target.read_text().splitlines()[0]
    assert first.index("Right column") > len("Left column") + 1


def test_no_target_uses_cwd(monkeypatch, tmp_path):
    paper(tmp_path / "one.pdf")
    paper(tmp_path / "nested" / "two.pdf")
    monkeypatch.chdir(tmp_path)
    assert engine.main([]) == 0
    assert (tmp_path / "one.txt").exists()
    assert not (tmp_path / "nested" / "two.txt").exists()


def test_recursive_folder_preserves_paths(tmp_path):
    folder = tmp_path / "input"
    paper(folder / "a.pdf")
    paper(folder / "nested" / "b.PDF")
    output = tmp_path / "output"
    assert engine.main([str(folder), "--recursive", "-o", str(output)]) == 0
    assert (output / "a.txt").exists()
    assert (output / "nested" / "b.txt").exists()


def test_existing_output_requires_explicit_overwrite(tmp_path):
    source = paper(tmp_path / "one.pdf")
    target = source.with_suffix(".txt")
    target.write_text("keep me")
    assert engine.main([str(source)]) == 1
    assert target.read_text() == "keep me"
    assert engine.main([str(source), "--overwrite"]) == 0
    assert "Left column" in target.read_text()


def test_failure_keeps_old_output_and_cleans_staging(tmp_path):
    source = tmp_path / "broken.pdf"
    source.write_bytes(b"not a PDF")
    target = source.with_suffix(".txt")
    target.write_text("keep me")
    assert engine.main([str(source), "--overwrite"]) == 1
    assert target.read_text() == "keep me"
    assert not list(tmp_path.glob(".tpe-*"))


def test_folder_continues_after_failure(tmp_path):
    (tmp_path / "a-broken.pdf").write_bytes(b"not a PDF")
    paper(tmp_path / "b-good.pdf")
    assert engine.main([str(tmp_path)]) == 1
    assert not (tmp_path / "a-broken.txt").exists()
    assert (tmp_path / "b-good.txt").exists()


def test_timeout_does_not_publish(monkeypatch, tmp_path):
    source = paper(tmp_path / "one.pdf")

    def timeout(command, timeout=120):
        raise subprocess.TimeoutExpired(command, timeout)

    monkeypatch.setattr(engine, "_run", timeout)
    assert engine.main([str(source)]) == 1
    assert not source.with_suffix(".txt").exists()
    assert not list(tmp_path.glob(".tpe-*"))


def test_url_uses_https_curl_and_real_poppler(monkeypatch, tmp_path):
    run = engine._run

    def download(command, timeout=120):
        if command[0] == "curl":
            assert command[1] == "--disable"
            assert "--globoff" in command
            assert command[command.index("--proto") + 1] == "=https"
            assert command[command.index("--proto-redir") + 1] == "=https"
            assert command[command.index("--max-filesize") + 1] == "104857600"
            assert command[-2] == "--"
            shutil.copyfile(FIXTURE, command[command.index("--output") + 1])
        else:
            run(command, timeout)

    monkeypatch.setattr(engine, "_run", download)
    url = "https://example.org/a%20paper.pdf?part=[1-2]"
    assert engine.main([url, "-o", str(tmp_path)]) == 0
    assert "Left column" in (tmp_path / "a paper.txt").read_text()


def test_failed_download_does_not_publish(monkeypatch, tmp_path):
    def fail(command, timeout=120):
        raise RuntimeError("download failed")

    monkeypatch.setattr(engine, "_run", fail)
    assert engine.main(["https://example.org/paper.pdf", "-o", str(tmp_path)]) == 1
    assert not list(tmp_path.iterdir())


def test_same_source_destination_rejected(tmp_path):
    source = paper(tmp_path / "one.pdf")
    before = source.read_bytes()
    with pytest.raises(ValueError, match="differ"):
        engine.convert_pdf(source, source, overwrite=True)
    assert source.read_bytes() == before


def test_empty_folder_and_missing_target_are_visible(tmp_path):
    with pytest.raises(SystemExit) as error:
        engine.main([str(tmp_path)])
    assert error.value.code == 2
    assert engine.main([str(tmp_path / "missing.pdf")]) == 1


@pytest.mark.parametrize(("mask", "mode"), [(0o022, 0o644), (0o077, 0o600)])
def test_publication_respects_umask_in_child_only(tmp_path, mask, mode):
    source = paper(tmp_path / "one.pdf")
    subprocess.run(
        [
            os.fspath(Path(sys.executable)),
            "-c",
            "from pdftextract_engine import convert_pdf; import sys; convert_pdf(sys.argv[1])",
            str(source),
        ],
        check=True,
        umask=mask,
    )
    assert source.with_suffix(".txt").stat().st_mode & 0o777 == mode


def test_invalid_url_reports_error_without_traceback(capsys):
    assert engine.main(["https://["]) == 1
    assert "Invalid IPv6 URL" in capsys.readouterr().err


def test_only_tpe_is_installed():
    from importlib.metadata import distribution

    scripts = distribution("pdftextract-engine").entry_points
    assert [entry.name for entry in scripts if entry.group == "console_scripts"] == ["tpe"]


def test_folder_output_collision_is_not_overwritten(tmp_path, capsys):
    upper = paper(tmp_path / "paper.PDF")
    lower = paper(tmp_path / "paper.pdf")
    if upper.samefile(lower):
        pytest.skip("Case-insensitive filesystem cannot represent this pair")
    lower.write_bytes(lower.read_bytes().replace(b"Left column", b"Second copy"))
    assert engine.main([str(tmp_path), "--overwrite"]) == 1
    assert "same output" in capsys.readouterr().err
    assert "Left column" in (tmp_path / "paper.txt").read_text()
