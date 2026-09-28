# Zotero integration

Three parts, all in this repository:

| Part | Where | What it does |
|------|-------|--------------|
| Web API client | `crates/tpe-zotero/src/client.rs` | Reads items, children and collections from zotero.org; creates items, notes and link attachments; versioned `PATCH` updates. |
| Local reader | `crates/tpe-zotero/src/local.rs` | Reads `~/Zotero/zotero.sqlite` read-only (works while Zotero is running) and resolves PDF paths under `storage/`. |
| Zotero plugin | `integrations/zotero-plugin/` | Adds **Send to Text Processing Engine** to the item context menu; posts the selection to the app's local server. |

Every Zotero item becomes a `tpe_common::PaperRecord` through one mapping
(`crates/tpe-zotero/src/item.rs`, `record_from_fields`), whatever the source.

## Mapping

| `PaperRecord` | Zotero |
|---------------|--------|
| `title` | `title` |
| `authors` | creators with `creatorType` = `author`: `name`, or `firstName lastName` |
| `year` | first four-digit year in `date` (free text or the stored `2019-00-00 2019` form) |
| `venue` | `publicationTitle` (journal article and default), `proceedingsTitle` then `conferenceName` (conference paper), `bookTitle` (book section), `publisher` (book), `institution` (report), `university` (thesis), `repository` (preprint) |
| `doi` | `DOI`, else a `DOI:` line in `extra` (normalised, lower case) |
| `arxiv_id` | an `arXiv:` line in `extra`, else `archiveID` (preprint) |
| `pmid`, `pmcid` | `PMID:` / `PMCID:` lines in `extra` |
| `url`, `abstract_text` | `url`, `abstractNote` |
| `source`, `source_id` | `"zotero"`, the item key |

Nothing is guessed: a missing field stays `None`. When writing a record back
(`ZItemPatch::from_record`), a name like `"Lovelace, Ada"` becomes
`firstName`/`lastName`; any other name is sent as a single-field `name` rather
than guessing where the family name starts. Identifiers without a dedicated
field go to `extra` (`arXiv: …`, `PMID: …`, `PMCID: …`, and `DOI: …` for
types other than journal article, conference paper and preprint).

## Web API setup

1. Find your numeric **user ID** on <https://www.zotero.org/settings/keys>
   (it is not your username). Group IDs come from `/users/<userID>/groups`.
2. Create a private key on the same page. Scopes:
   * **Allow library access**: required for reading a private library.
   * **Allow notes access**: required to read notes and to create notes
     (`create_note`).
   * **Allow write access**: required for `write_items`, `update_item`,
     `create_note` and `add_attachment_link`.
   * Groups: *read only* or *read/write* per group, as above.
   A read-only workflow needs only library access.
3. Store the key with `tpe-credentials` under the `ZOTERO` service name. Never
   put it on the command line, in logs or in the ledger. `ApiKey`'s `Debug`
   prints `ApiKey(***)`.

### Protocol details the client follows

Checked against the Zotero Web API documentation (basics and write-request
pages, last updated 2026-07-29):

* Base URL `https://api.zotero.org`; libraries are `/users/<id>` or `/groups/<id>`.
* Every request sends `Zotero-API-Version: 3`; the key goes in the
  `Zotero-API-Key` header (not the URL), so the `Link` URLs can be followed
  unchanged.
* Multi-object reads return `Total-Results`, `Last-Modified-Version` and
  `Link` (`rel="next"`, `"last"`, ...). `limit` is 1–100, default 25.
  `since=<version>` fetches only changed objects. The client follows
  `rel="next"` only when it points inside the API base, so the key is never
  sent to another host.
* `Backoff: <seconds>` can appear on any response (exposed as
  `Page::backoff_secs`); `429` carries `Retry-After`
  (`ZError::RateLimited`). Make at most 4 concurrent requests.
* `POST <prefix>/items` creates up to **50** objects per request. Unversioned
  writes carry a fresh 32-character `Zotero-Write-Token`, so a retried request
  is not applied twice (the server remembers tokens for 12 hours and answers
  `412` on reuse). The 200 response maps array indexes to results under
  `successful` / `success`, `unchanged` and `failed`.
* `PATCH <prefix>/items/<key>` with `If-Unmodified-Since-Version: <item version>`
  answers `204`, or `412` if the item changed since it was read (re-read,
  then retry).
* Child notes and attachments are created with `parentItem`. New items carry
  `itemType`, `tags`, `collections` and `relations`.
* `/items/<key>/file` serves an attachment's file (`attachment_file_url`).

The Zotero desktop app also serves the same read API locally at
`http://localhost:23119/api/` when enabled in its settings. Point the client at
it with `ZoteroClient::with_base("http://localhost:23119/api")`. Local writes
need Zotero 10+ and a separately granted local key.

## Local database

`tpe_zotero::local::find_zotero_dir()` uses `$TPE_ZOTERO_DIR` if set,
otherwise `~/Zotero` (the default data directory on macOS and Linux). A
custom data directory set in Zotero's advanced settings must be passed
explicitly.

Zotero holds an exclusive lock on `zotero.sqlite` while it runs. The reader
opens `file:<path>?immutable=1` with `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_URI`,
so SQLite takes no locks and never writes. Recent changes that Zotero has not
yet written into the main database file may be missing until Zotero
checkpoints or quits. Do not copy the file while Zotero writes to it.

Tables read (a subset modelled on the Zotero 7 layout, not copied from
Zotero's schema file): `items`, `itemTypes`, `fields`, `itemData`,
`itemDataValues`, `creators`, `creatorTypes`, `itemCreators`,
`itemAttachments`, `collections`, `collectionItems`, `deletedItems`. Notes,
annotations, attachments and trashed items are not returned as items.

Attachment paths:

| `itemAttachments.path` | Link mode | Resolved to |
|------------------------|-----------|-------------|
| `storage:<file>` | stored file / snapshot | `<data dir>/storage/<attachment key>/<file>` |
| absolute path | linked file | that path |
| `attachments:<relative>` | linked file under the base directory | not resolved (depends on a Zotero preference); raw value kept |
| URL | linked URL | none |

## Plugin

Zotero 7 bootstrap plugin, no build step:

```
integrations/zotero-plugin/
  manifest.json            manifest_version 2, applications.zotero {id, update_url, strict_min_version 7.0, strict_max_version 7.*}
  bootstrap.js             startup / shutdown / onMainWindowLoad / onMainWindowUnload
  prefs.js                 default prefs: port 47821, empty token
  locale/en-US/tpe-zotero.ftl
  updates.json             update manifest (no published updates yet)
  make-xpi.sh              zips the files above into dist/tpe-zotero.xpi
```

Install: run `integrations/zotero-plugin/make-xpi.sh`, then in Zotero
choose *Tools → Plugins → gear menu → Install Plugin From File…* and select the
`.xpi`. Set `extensions.tpe-zotero.token` (and, if needed,
`extensions.tpe-zotero.port`) in *Settings → Advanced → Config Editor*. The
plugin will not send anything while the token is empty. Zotero stores
preferences as plain text in the profile, so use a random token that is only
used for this.

## Local server contract (implemented by the app track)

```
POST http://127.0.0.1:<port>/zotero/import      (default port 47821)
Content-Type: application/json
X-TPE-Token: <shared token>
```

The server must:

* listen on `127.0.0.1` only;
* compare `X-TPE-Token` in constant time and answer `401` when it is missing
  or wrong (the plugin reports the status code);
* answer `200`, `201`, `202` or `204` on success; `400` when the body does not
  parse or `schema` is unknown;
* treat attachment paths as untrusted input: read only regular files, apply
  the engine's `--max-bytes` limit, and never follow them to write anything.

Body (Rust types: `tpe_zotero::import::{ImportRequest, ImportItem, ImportAttachment}`;
parse with `ImportRequest::parse`, map with `ImportItem::to_record`):

```json
{
  "schema": "tpe.zotero.import/1",
  "plugin_version": "0.1.0",
  "zotero_version": "7.0.11",
  "items": [
    {
      "key": "ABCD2345",
      "library_id": 1,
      "data": { "itemType": "journalArticle", "title": "…", "creators": [], "DOI": "…", "extra": "…" },
      "attachments": [
        {
          "key": "PDFK2345",
          "title": "Full Text PDF",
          "content_type": "application/pdf",
          "link_mode": 0,
          "path": "/Users/me/Zotero/storage/PDFK2345/paper.pdf",
          "url": null
        }
      ]
    }
  ]
}
```

* `data` is Zotero's `item.toJSON()`: the same editable JSON as the Web
  API's `data` property.
* `link_mode`: 0 imported file, 1 imported URL (snapshot), 2 linked file,
  3 linked URL.
* `path` is `null` when the file is not on this computer (for example not
  yet synced) or for linked URLs.
* Only regular items are sent; selected notes and attachments are skipped.
