# Web alpha source and Site exchange

`web/` contains the private extraction alpha's application source, browser PDF
worker, HTML/feed extraction, owner-scoped API, database schema and migrations,
UI components, build adapters, and pinned pnpm lockfile. It is a source mirror of
the Site, not a copy of its user documents, credentials, or deployment identity.
The native Rust engine and this browser application remain separate runtimes.
See `web/README.md` for the implemented features and their limits.

The initial mirror was captured after Site source commit
`3b00638a24144c6ff4546e6ee43fab597c050678`. The updated 153-file mirror includes
the upload, OCR, Office, archive, reader and MCP implementation. See
[HANDOFF_TO_DOT.md](HANDOFF_TO_DOT.md) for the final publication checkpoint.
`web/SOURCE_MANIFEST.json` records each current source file, size, normalized mode
and SHA-256; its complete allowlist was round-trip compared with the Site checkout.
The manifest is deterministic: it contains no timestamps, machine paths, or Site
identifiers. `.gitignore` receives a deterministic safety appendix so generated
WASM, local deployment settings, and build state stay out of Git, while the web
build adapter source remains trackable inside the Rust repository.

## Verify and export

From the repository root:

```sh
uv run python scripts/site_source.py verify --source web
uv run pytest web/tests/test_site_source.py
uv run python scripts/site_source.py export \
  --source /path/to/site-checkout --output /path/to/new-source-bundle
```

The output directory must not exist, its parent must already exist, and it must
be outside the source tree. Export reads the allowlisted source twice and refuses
a changed snapshot. It copies regular files only and writes a SHA-256 manifest.
Existing outputs, symlink paths, special files, oversized bundles, missing core
configuration, and recognizable private-key material are rejected.

The allowed roots are `app`, `components`, `lib`, `db`, `drizzle`, `public`,
`scripts`, `vendor`, `build`, `hooks`, and `docs`, plus explicitly named project
configuration files and the lockfile. `.openai`, `.env*` (including examples),
`.npmrc`, `node_modules`, `dist`, `.sites-runtime`, `.wrangler`, `.git`, databases,
credential filenames, generated declarations/caches, and
`public/vendor/pdf-oxide` and `public/ocr` are excluded. Application source must still be reviewed
for inline secrets; filename filtering and checksums are not a secret scanner or
an authenticity signature.

## Edit and stage an import

Edit `web/` normally in Git. Verification will report a mismatch until the edited
source is exported with a new manifest. Create a fresh bundle, then stage it for
comparison with an existing Site:

```sh
uv run python scripts/site_source.py export \
  --source web --output /path/to/edited-web-bundle
uv run python scripts/site_source.py import \
  --source /path/to/edited-web-bundle \
  --output /path/to/new-import-stage \
  --against /path/to/site-checkout
```

Import verifies every allowlisted file's bytes, size, and mode before writing.
It creates a new source directory and `IMPORT_PLAN.json`, listing additions,
modifications, unchanged files, and target-only files to retain. `--against` is
read-only. The script never applies changes to an existing checkout, deletes a
target-only file, starts a preview, deploys, or touches deployment credentials.
Review the staged source and plan, then let the Site owner integrate the intended
files through the Site's normal editing and deployment workflow. In particular,
review build/configuration changes against the destination's current adapters.

The `web/tests/` regression suite belongs to this repository's exchange tooling;
it is not application source and is intentionally omitted from export/import.
Ignored files added beside a bundle are never imported. Extra or missing files
inside the source allowlist cause verification to fail.

## Local setup and deployment boundary

Use Node and pnpm versions declared in `web/package.json`. The lockfile pins
dependencies; install with a frozen lockfile. The Site's managed configuration is
intentionally absent. For an isolated local preview, create an ignored
`web/.openai/hosting.json` containing only local binding names:

```json
{"d1":"DB","r2":"BUCKET","capabilities":["mcp"]}
```

Do not replace an existing hosting file. In a managed Site, let Sites provision
that file and its actual D1/R2 bindings. From `web/`, the build commands are:

```sh
pnpm install --frozen-lockfile
node scripts/copy-pdf-wasm.mjs
node scripts/copy-ocr-assets.mjs
pnpm build
```

The copy scripts recreate the PDF Oxide JavaScript/WASM and Tesseract OCR
worker/core/English-model assets from pinned installed packages. Framework builds regenerate `next-env.d.ts` and route types.
Database migrations are under `drizzle/`; the deployment owner must apply them to
the intended database. Never commit local database contents or object storage.

**Authentication is deployment-specific.** `app/chatgpt-auth.ts` trusts the
`oai-authenticated-*` identity headers supplied by the trusted Sites edge.
`lib/server.ts` then scopes database and object access to that verified owner.
This header adapter must not be exposed behind an arbitrary public server where
clients can supply those headers. An alternative host needs its own verified
session/authentication adapter and equivalent owner isolation. Portable preview
mode includes mock authentication for isolated local development; never expose
that preview as a public deployment. The build's Cloudflare/D1/R2 and Sites
adapters are included so their boundary is visible, not silently bypassed.

The source mirror does not export private user data. Moving an application's
source between Sites does not migrate existing documents, buckets, database
ownership, or authentication identities.
