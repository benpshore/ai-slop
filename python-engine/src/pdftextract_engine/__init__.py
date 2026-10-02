"""Convert PDFs with pdftotext -layout: tpe [URL, PDF or folder]; default: cwd."""

import argparse
import os
import subprocess
import sys
import tempfile
from importlib.metadata import version
from pathlib import Path
from urllib.parse import unquote, urlsplit


def _run(command, timeout=120):
    result = subprocess.run(
        command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=timeout, check=False
    )
    if result.returncode:
        raise RuntimeError(result.stderr.decode("utf-8", errors="replace").strip())


def convert_pdf(source, destination=None, *, overwrite=False, timeout=120):
    """Publish successful layout text atomically; preserve existing output by default."""
    source = Path(source).resolve()
    destination = Path(destination) if destination else source.with_suffix(".txt")
    if not source.is_file() or source.suffix.lower() != ".pdf":
        raise ValueError(f"Not a PDF file: {source}")
    if source == destination.resolve():
        raise ValueError("Output must differ from the input PDF")
    if not overwrite and destination.exists():
        raise FileExistsError(f"Output already exists: {destination}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    # Poppler creates normal umask-constrained output inside this private directory.
    with tempfile.TemporaryDirectory(prefix=".tpe-", dir=destination.parent) as folder:
        staged = Path(folder) / "text.txt"
        _run(["pdftotext", "-layout", "-enc", "UTF-8", str(source), str(staged)], timeout)
        if overwrite:
            os.replace(staged, destination)
        else:
            os.link(staged, destination)  # No-clobber even if another writer races us.
    return destination


def convert_url(url, output_dir=Path("."), *, overwrite=False):
    """Download a direct PDF over HTTPS, then use the same local conversion path."""
    name = Path(unquote(urlsplit(url).path)).stem or "download"
    destination = Path(output_dir) / f"{name}.txt"
    with tempfile.TemporaryDirectory(prefix="tpe-download-") as folder:
        pdf = Path(folder) / "input.pdf"
        command = (
            "curl --disable --fail --silent --show-error --location --proto =https "
            "--proto-redir =https --max-time 110 --max-filesize 104857600"
        ).split()
        _run([*command, "--output", str(pdf), "--", url])
        return convert_pdf(pdf, destination, overwrite=overwrite)


def main(argv=None):
    parser = argparse.ArgumentParser(prog="tpe", description=__doc__)
    parser.add_argument("target", nargs="?", default=".")
    parser.add_argument("--version", action="version", version=version("pdftextract-engine"))
    parser.add_argument("-o", "--output-dir", type=Path)
    parser.add_argument("--recursive", action="store_true")
    parser.add_argument("--overwrite", action="store_true")
    args = parser.parse_args(argv)
    remote = urlsplit(args.target).scheme in {"https", "http"}
    source = Path(args.target)
    base = source if source.is_dir() else source.parent
    pattern = "**/*" if args.recursive else "*"
    files = [source]
    if not remote and source.is_dir():
        files = sorted(
            p for p in source.glob(pattern) if p.is_file() and p.suffix.lower() == ".pdf"
        )
    if not files:
        parser.error(f"No PDFs found: {source}")
    failed = 0
    for pdf in files:
        try:
            if remote:
                target = convert_url(
                    args.target, args.output_dir or Path("."), overwrite=args.overwrite
                )
            else:
                target = args.output_dir / pdf.relative_to(base) if args.output_dir else pdf
                target = target.with_suffix(".txt")
                convert_pdf(pdf, target, overwrite=args.overwrite)
            print(target)
        except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired) as error:
            failed += 1
            print(f"{args.target if remote else pdf}: {error}", file=sys.stderr)
    print(f"Converted: {len(files) - failed}; failed: {failed}", file=sys.stderr)
    return 1 if failed else 0
