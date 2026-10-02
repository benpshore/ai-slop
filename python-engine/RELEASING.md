# Python patch releases

Tracks [#164](https://github.com/benpshore/pdftextract/issues/164) and
[#165](https://github.com/benpshore/pdftextract/pull/165). The core is verified on
ARM Linux and Darwin with real Poppler, a built-wheel uv tool installation, and
an actual HTTPS PDF. No Python GitHub release has been published yet.

First merge the reviewed 0.0.1 package and CI. To prepare a later version, start
with a clean review branch and run:

```sh
uv run --directory python-engine python scripts/release.py
```

The small helper guards the branch/clean tree and runs
`uv version --bump patch`. Review its pyproject.toml and uv.lock changes in a PR;
it never pushes a branch or changes protected main. After the version PR merges,
tag that reviewed main commit `python-v0.0.N`. The first tag is `python-v0.0.1`.
Pushing the tag automatically runs Python release publication. No bot version
controller or automatic main version mutation is introduced.

The workflow requires the tag/package version to match, the next 0.0.x patch,
main ancestry, exact tag commit identity, and successful main-push Python engine
CI on that same commit. Dispatch can retry a failed, unpublished current tag;
an older tag is rejected once a higher patch exists.
Publication is serialized. Runtime/dev dependencies are checked with locked uv;
uv and its build backend are pinned to 0.12.19, Python to 3.14.7. Build time is
fixed to the source commit timestamp. The actual wheel is installed as a uv tool
and its `tpe --version` is checked before wheel/sdist/checksums are uploaded.
Python tags do not replace the existing Rust latest-release contract.

Install a released wheel with `uv tool install --python 3.14.7 <wheel-URL>`;
Poppler remains an external prerequisite. Only `tpe` is installed as a command.
