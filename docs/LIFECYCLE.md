# Update and release lifecycle

Checked **2026-09-30** against the live repository, GitHub's docs and the upstream
sources named below. Status words are literal: **active** runs now, **verified**
means I ran or read it, **unverified** means it can only be confirmed when GitHub
runs it, **design** is a proposal with no code, **planned** is not started.

Nothing here enables auto-merge, changes the branch ruleset, or weakens the
single required check `ci`. Every update is a PR that a human merges.

## Who does what

| Concern | Mechanism | Status |
| --- | --- | --- |
| Cargo, uv and Actions versions | Dependabot, weekly, grouped, 7-day cooldown (`.github/dependabot.yml`) | active |
| Actions pinned by full SHA plus `# vX.Y.Z` | Dependabot keeps both; `tests/test_lifecycle_watch.py` fails `ci` on an unpinned action, container image or unhashed native artifact | active, verified |
| Pinned SHA really is the tag it names | `lifecycle-watch.yml`, daily, fails the run on a mismatch | active, unverified until first run |
| PDFium binaries, model releases | `lifecycle-watch.yml` opens one issue per candidate past the cooldown | active, unverified until first run |
| Runner image deprecation, `ubuntu-latest` moving | same workflow, from the `actions/runner-images` table | active, unverified until first run |
| Swift job container image | manual (Dependabot cannot update job containers); digest checked below | manual |
| Rust toolchain | floats with the runner image; no pin | decision 3 |
| Turning a candidate into a PR (bump, hashes, gates) | by hand or by an agent, from the issue | planned |
| Release notes layout | `.github/release.yml` | active, unverified until next release |
| Release signing provenance, app self-update | see below | design |

## State found on 2026-09-30

- Dependabot was already configured (weekly, 7-day cooldown, one all-in-one group
  per ecosystem). Its runs succeed; it has opened one PR (#13, `time`), closed
  the same day. Nothing is red on `main`.
- 40 PRs are open, most from agents. **#83** (open, stacked on #78) adds a daily
  update workflow for Docling, PDFium and models. Read it before merging: it runs
  on `macos-latest` on a schedule (the pattern the repo removed after a macOS
  death loop), removes Dependabot's cooldown, and proposes the newest release the
  day it appears, which is the window a compromised release is worst in. Its own
  docs say it also needs the repo setting "Allow GitHub Actions to create pull
  requests".
  This PR does not touch it; see decision 1.
- **#95** rewrites `auto-release.yml` to keep the write token away from build
  code. This PR does not touch that file, so the two cannot conflict.
- `actions/cache` is pinned at v4.3.0 (latest v6.1.0) and `actions/upload-artifact`
  at v4.6.2 (latest v7.0.1); checkout, setup-uv and codeql-action are current.
  Dependabot will open those once its cooldown allows; read their release notes,
  these are major bumps.
- Identity constants (`LOPDF_VERSION`, `PDFIUM_RENDER_VERSION`, `DOCLING_VERSION`,
  `PDFIUM_BINARY_VERSION` in `src/backend/`) are tested against `Cargo.lock`. A
  Dependabot PR that moves one of those crates is **red by design** until someone
  edits the constant. That is why they sit in their own Dependabot group.
- `ci` aggregates `repo`, `python`, `rust` (x86-64 and ARM64 Linux) and `swift`; no
  macOS runner is in it.

## Supply-chain rules

1. Pin by immutable identity: full commit SHA for actions, sha256 digest for
   container images, sha256 (archive and installed file) for native artifacts,
   `Cargo.lock` and `uv.lock` checksums for packages. Enforced in `ci`.
2. Cooldown: a release is not proposed until it is 7 days old (14 for major
   Cargo and uv bumps). Dependabot applies it to versions; `lifecycle-watch.yml`
   applies it to PDFium (release publication date) and **fails closed**: a date it
   cannot read means no proposal. GitHub exempts security updates from cooldown,
   which is what you want.
3. Group to limit noise: one PR each for Actions, uv, routine Cargo (minor and
   patch), the extractors (`docling*`, `pdfium-render`, `lopdf`), and `gpui`. A
   major bump of any other crate is its own PR.
4. Never copy a digest from a file served beside the artifact; compute it from
   the bytes you pinned. `check_native_manifest` refuses a null hash.
5. Text from upstream (tags) reaches an issue only after a strict pattern match.
   The workflow has `contents: read` and `issues: write`, no other secret, and
   does not run on forks.

### Not covered (residual risk)

- A compromised Actions release that is more than 7 days old when proposed.
  Read the diff of every action bump; SHA pinning stops silent tag moves, not a
  malicious release.
- PDFium binaries are a third-party build (`bblanchon/pdfium-binaries`), and the
  ONNX Runtime download inside the `ort` crate build script is not pinned by us.
- Wheel, sdist, `tpe` binary and app zip have no signature or attestation yet
  (see Provenance).
- `#83`-style automation that runs repository code with a write token.

## Reading an update PR

1. Check the PR is from `dependabot[bot]` and the branch is `dependabot/...`.
2. Read the upstream release notes for every bumped package; for an Action, read
   the diff between the two SHAs.
3. `ci` must pass. For an `extractors` PR, edit the identity constants and push
   to the branch (or comment `@dependabot recreate` after fixing `main`), and run
   the Native workflow: changed extractors change the processing identity, so old
   results keep their old provenance.
4. `gpui` PRs must also pass the App workflow; then run the app once on a Mac.
5. Merge. Every merge releases.

## Runbook: a bad update

Before merge:
- Close the PR and tell Dependabot why: `@dependabot ignore this minor version`
  (or `major`, `patch`, `dependency`); `@dependabot unignore <name>` reverses it.
  These commands are in GitHub's docs (checked today).
- Candidate issues from `lifecycle-watch`: close the issue. That records the
  decision; only a newer candidate opens another.

After merge, before release problems appear:
- Revert with a PR (`git revert -m 1 <merge commit>`), never a force push. The
  revert is itself released, which gives consumers a fixed version.

After a release is bad:
1. Roll forward with the revert above; do not delete or move the tag (tags are
   the audit trail and pinned consumers may rely on them).
2. Stop new consumers picking it up: mark the release a pre-release and mark the
   previous good one latest (`gh release edit vX.Y.0 --prerelease`, then
   `gh release edit vPREV --latest`; flags from gh's manual, unverified here
   because gh is not installed in this sandbox). Delete a poisoned asset only if
   it is dangerous, and say so in the release notes.
3. Native pins (PDFium, models): revert the `native/manifest.json` PR;
   `native/fetch.sh` provisions the old, hash-verified files. Results produced
   under the bad pin keep their recorded processing identity.

## Releases

**How a change becomes a release.** Merge to `main`, `ci` passes, `auto-release`
tags the next minor (`git tag --list 'v*'` latest plus one) and attaches wheel and
sdist. `release.yml` only runs when a person pushes a `v*` tag (tags made with
`GITHUB_TOKEN` do not trigger workflows), and additionally builds a `tpe` binary.
The App release workflow proposed in #110 attaches the app to the latest release
by hand.

**Noise, measured.** Tag `v0.41.0` was published 2026-09-29 22:43 UTC, about 38
hours after the workflows were created (2026-09-28 08:12 UTC); eight of them
(v0.34.0 to v0.41.0) fell between 02:31 and 22:43 on the 29th. The wheel is a
small Python package with no dependencies that nothing installs, and the minor
number is a build counter.

| Option | Effect | Cost |
| --- | --- | --- |
| A. Keep (today) | every merge addressable and revertable | 8 releases on 2026-09-29, each with its own notes |
| B. Mark auto-releases pre-release | list still noisy; "latest" empties, breaking `gh release view` in #110 | breaks a flow |
| C. Tag every merge, publish a Release only on demand (`--notes-start-tag` the last one) | quiet list, real notes | changes the AGENTS.md rule; needs a promote workflow |
| D. Patch bumps for merges, minor on promotion | meaningful numbers | changes version semantics |

**Recommendation: keep A now.** It is the required flow, it costs nothing, and the
notes are now grouped by label (`.github/release.yml`: breaking, security,
upstream, dependencies, changes; `skip-changelog` hides a PR). The app must not
depend on the release cadence at all, which is why the self-update design uses a
separately promoted channel. **Strongest case for C:** a release list nobody reads
hides the one that matters. **What settles it:** whether anything installs from
Releases. If the app channel below ships, release noise stops mattering; if not,
adopt C. Note the config is read when notes are generated; whether GitHub reads
`release.yml` from the tag or from the default branch is unverified until the next
release.

## Deprecation and end of life

| Item | Found 2026-09-30 | Trigger | Automated |
| --- | --- | --- | --- |
| GPUI | `gpui = "0.2"` locked 0.2.2; crates.io newest is 0.2.2 (2025-10-22) | Dependabot `gpui` group PR when a new line is 7 days old | yes |
| pdfium-render | locked 0.8.37; 0.9.4 exists; docling-pdf 1.69.2, 1.74.0, 1.74.1 require `^0.8` (crates.io index) | Dependabot ignore rule expires when Docling accepts 0.9 | ignore rule is manual |
| Rust | unpinned; newest stable 1.98.1 (2026-09-03, endoflife.date); edition 2024 needs 1.85+; no MSRV declared | only latest stable is supported upstream | no (decision 3) |
| `ubuntu-latest` | Ubuntu 24.04; `ubuntu-26.04` is available (runner-images table) | `lifecycle.json` `runner_latest` differs from the table | yes |
| `ubuntu-24.04-arm`, `macos-15` | supported; `macos-14` is marked deprecated | a used label marked deprecated | yes |
| macOS SDK | `macos-15` image default Xcode 16.4, SDK macOS 15.5; app `LSMinimumSystemVersion` 14.0; macOS 26 image and Xcode 27 preview exist | manual, per macOS release | no |
| Swift job image | `swift:6.4.0-noble@sha256:64bab7...` equals the digest of `6.4-noble` and `noble` on Docker Hub today; newest `-noble` tag is 6.4.0 | quarterly, by hand | no |
| PDFium binaries | pinned `chromium/8066`; tag `chromium/8076` exists (release date not readable from here) | issue when 7 days old | yes |

## Provenance of pinned artifacts

Present: hashes in `native/manifest.json` (archive and installed file), Cargo and
uv lockfile checksums, action SHAs verified against upstream tags daily.
Missing: attestations for the wheel, sdist, `tpe` binary and app zip; an SBOM.
Proposal (design): after #95 lands, add a build-provenance attestation step to
`auto-release.yml` (needs `id-token: write` and `attestations: write` on the
publish job only, and a pinned action whose SHA is verified when written), so a
consumer can run `gh attestation verify`. Not done here: it edits the file #95
rewrites, and provenance for the app is worth less than the signed manifest
below, which also covers users who never see GitHub.

## Design: self-update for the macOS app (not implemented)

Status: design only. Nothing here changes `crates/tpe-app`. Every claim about
macOS behaviour marked (Mac) is reasoned, not run; the app has not been run on a
Mac (docs/APP.md), so each needs a check on real hardware first.

### What signing and notarization change

The bundle is ad-hoc signed and not notarized; notarization needs an Apple
Developer ID and is Ben's decision. It is not faked here.

| Question | Without Developer ID | With Developer ID and notarization |
| --- | --- | --- |
| First install from a browser | Quarantined. Apple's page lists one route: System Settings, Privacy & Security, Open Anyway, then the login password; "This button is available for about an hour after you try to open the app." That is a timed, multi-step flow. `docs/APP.md` and #110 say right-click, Open; Apple's page does not mention it, so treat that text as stale until checked on macOS 15 and 26 | opens normally |
| Is an in-app update trusted? | Only by our own Ed25519 check; macOS cannot tell it is the same developer | same, plus Gatekeeper |
| Does macOS keep permissions across updates? (Mac) | Probably not: an ad-hoc signature has no stable identity, so Files & Folders grants and Keychain item access may be asked again after every update; each is a system dialog | stable identity, grants persist |
| Can the app replace itself? | Yes, if it downloaded the file itself (a file the app writes is not quarantined unless the app opts in via `LSFileQuarantineEnabled`; unverified here) and it is not running from a translocated path (launched from Downloads) or a read-only location | yes |
| Off-the-shelf Sparkle | Its docs treat Developer ID as the distribution path; ad-hoc is mentioned only as a development caveat | supported |

The strongest accessibility argument for notarizing is the first row: the Open
Anyway flow is the hardest step for anyone with limited hand function, and it is
timed. **Recommendation: get the Developer ID.** Until then, ship step 2 below
(notify only) and no installer.

### Threat model and key custody

Anyone who can merge to `main` can cause a release (agents included), so the
update trust root must not live in Actions. The Ed25519 signing key stays offline
on Ben's Mac (or a hardware token). CI can never sign. A compromised CDN or
release upload can only cause a signature failure or deny updates.

### Channels and manifest

Two channels, both opt-in: `stable` and `beta`. No nightly. A channel is one JSON
manifest plus a detached signature, published by Ben after he has run the App
workflow artifact by hand. It is independent of the ~8 daily auto-releases.

```json
{
  "schema": 1, "channel": "stable", "sequence": 17,
  "issued": "2026-10-05T00:00:00Z", "expires": "2026-11-05T00:00:00Z",
  "key_id": "2026a",
  "release": {
    "version": "0.52.0", "min_macos": "14.0", "arch": "arm64",
    "url": "https://github.com/benpshore/pdftextract/releases/download/v0.52.0/PDFTextract-0.52.0-macos-arm64.zip",
    "sha256": "<64 hex>", "bytes": 12345678, "notes_url": "https://github.com/benpshore/pdftextract/releases/tag/v0.52.0"
  },
  "revoked_versions": ["0.50.0"]
}
```

- Signature: Ed25519 over the exact manifest bytes (`stable.json.sig`). The app
  embeds two public keys (current and next) so a key can rotate; if both are
  lost, users reinstall by hand, and the doc says so.
- Verification code: prefer a verify-only crate. `minisign-verify` 0.3.0 (crates.io,
  updated 2026-09-25) is verification-only; signing uses the `minisign` tool on
  Ben's Mac. `ed25519-dalek` 3.0.0 is the general alternative. Either adds one
  dependency to audit; that is the price of not trusting the network.
- `sequence` only increases: the app refuses an older manifest than the highest it
  has seen (a replayed old manifest cannot push users back). A user-chosen revert
  (below) does not consult it.
- `expires` limits how long a stale manifest is believed. It is never shown as a
  countdown and never blocks use; an expired manifest only reads "update
  information is out of date" as passive text. An already verified download is
  installed regardless of later expiry, so nobody can be timed out mid-decision.
- Hosting: an orphan branch `updates` (only Ben may push, history is the audit
  log) or a fixed release tag with replaced assets. Recommend the branch.

### Install and rollback

1. Download to a staging directory on the same volume as the app, check `bytes`
   and `sha256` against the signed manifest, unpack with `ditto -x -k`, run
   `codesign --verify --strict`, check bundle id and version equal the manifest.
2. Swap with an atomic rename (`renamex_np` with `RENAME_SWAP`; APFS, (Mac)); the old bundle
   then sits in staging and is **kept** as the previous version.
3. Relaunch only when the person chooses it.
4. If the location is not writable (standard user, translocated), do not ask for
   privileges and do not install a privileged helper; open the download folder
   and say why. (Mac) Detect translocation by an `AppTranslocation` path.
5. Rollback is the retained previous bundle plus a "Revert to previous version"
   menu item. There is deliberately no automatic crash rollback: it needs a
   separate always-good launcher, which is more code and attack surface than the
   problem is worth. A build that cannot launch is undone in Finder by opening
   the kept copy; the release assets are the fallback.

### Opt-in, accessible behaviour (motor neuron disease, neuromuscular conditions)

- No network request at launch. "Check automatically" is a setting, **off** by
  default; when on, at most once a day, only while idle, silent when current.
- "Check for Updates..." is a menu item and a button. The answer appears in a
  persistent, non-modal status row that stays until dismissed. No toast, no timer,
  no countdown, no auto-dismiss, no "installing in 10 seconds", no forced quit.
- Each step is a separate, unhurried choice: Download, then Install, then
  Relaunch. Each has Cancel; a confirmation waits indefinitely and Escape cancels.
  "Skip this version" and "Not now" are permanent until the next manual check:
  no reminder schedule.
- Never installs or relaunches with queued or running jobs; "install when idle"
  exists only as something the person turns on for that update.
- Everything is reachable by keyboard with one press per action (no chords, no
  press-and-hold, no drag, no hover-only). Reuse the app's large targets.
- Failure never blocks the app: it keeps running the current version.
- No identifiers are sent: a plain GET of a static file; GitHub sees an IP.
- **Screen readers.** GPUI 0.2.2 exposes no accessibility tree (docs/APP.md), so a
  custom GPUI update panel would be invisible to VoiceOver. Present the
  "update available" step in a native AppKit alert or panel, which has
  accessibility for free, and never time it out. Unverified until run with
  VoiceOver.

### Build order (each its own PR)

0. Ben decides Developer ID and key custody.
1. Manifest schema, `minisign` signing notes, verify library with test vectors.
   No UI, no network.
2. Manual "Check for Updates..." that verifies and **only tells** (opens the
   release page). Needs no Developer ID and cannot damage an install. Stop here
   until the Developer ID decision.
3. Staged install with retained previous bundle and revert.
4. Opt-in automatic check.

Checks that need a Mac first: quarantine on app-written files; whether an
ad-hoc update re-triggers Files & Folders and Keychain prompts; the Open Anyway
window; translocation; VoiceOver on the alert.

**Strongest case for Sparkle instead:** it is mature and avoids writing swap and
rollback code ourselves, and its native UI is accessible. **Against:** its
documented defaults check every 24 hours and ask permission at the second launch
(both need configuring away), it documents Developer ID, and it embeds a large
Objective-C framework in a Rust bundle. **What settles it:** after the Developer
ID exists, a one-day spike: does Sparkle load in the bundle, can every automatic
behaviour be switched off, is its window usable with VoiceOver.

## Decisions for Ben

1. **#83 (custom daily updater).** Recommend: do not merge as written. Keep its
   PDFium/model/Docling logic if wanted, but run it on `ubuntu-24.04-arm`, with a
   cooldown, without removing Dependabot's cooldown. This PR's issue-only watcher
   is the smaller alternative. Settled by: whether you want PRs prepared for you
   or issues to act on. Default implemented: issues.
2. **Cooldown lengths.** 7 days (14 for major). Longer is safer against a
   compromised release, and slower for fixes GitHub does not flag as security
   updates. Changing it is a one-line edit.
3. **Pin the Rust toolchain?** Recommend yes, via `rust-toolchain.toml` and
   Dependabot's `rust-toolchain` ecosystem (it supports cooldown), so a runner
   image update cannot change what `ci` builds. Not done: it also changes every
   local `cargo` on the shared, nearly full build machine, and I could not test
   how the runner's rustup reacts. Counter-case: floating gets fixes for free.
   Settled by trying it on a branch.
4. **Release frequency.** Recommend A now, C if nothing consumes Releases.
5. **Notarization.** Recommend yes; see the table above.
6. **Dependabot vs Renovate.** Recommend staying: it is already enabled, sends
   security alerts, and honours cooldown and groups. Renovate is more
   configurable (custom managers for `native/manifest.json`); consider it only if
   the manifest needs PRs rather than issues.
7. **CODEOWNERS** was not added: if the ruleset requires code-owner review, PRs
   authored through Ben's own account could not be approved by anyone.

## Cost of scheduled workflows

| Workflow | Runs | Runner | Time | Notes |
| --- | --- | --- | --- | --- |
| `lifecycle-watch.yml` | daily | 1 ubuntu-latest job | script 4.84 to 6.70 s (n=8, median 5.83 s, max/median 1.15, sandbox measurement); plus checkout and setup-uv, a few seconds in the CI logs | not on forks; `contents: read`, `issues: write` |
| Dependabot | weekly | GitHub-hosted, free | seconds | its PRs run `ci` (about 90 s) and App (macOS) when `Cargo.lock` changes |
| CodeQL, Eval | existing | ubuntu | unchanged | not touched |

## Deliberately not added

No PR-creating bot (a token that pushes branches is attack surface and the
`GITHUB_TOKEN` cannot start `ci` on its own PRs); no auto-merge; no macOS job; no
docker, Rust, Poppler or MLX watcher (Dependabot covers crates; Poppler and MLX are
not adopted, so there is nothing to update); no change to `ci.yml`,
`auto-release.yml`, `native.yml` or any Cargo, Swift or app file; no CODEOWNERS; no
`rust-toolchain.toml`; no attestations; no self-update code.
