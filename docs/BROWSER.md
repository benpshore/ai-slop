# Research browser (`tpe-browser`): Chromium embedding spike

Status: **model + design**. The crate `crates/tpe-browser` compiles on every CI
target with no native dependency and delivers the unit-tested *decision layer*
a Chromium shell needs. The Chromium (CEF) embedding itself is **not**
implemented: it is designed here and reserved behind the `cef` feature as
documented TODOs. Nothing in the crate loads or renders a page.

Evidence policy for this document: statements about *this crate* point at a
test or a source line. Statements about CEF come from the CEF C/C++ API, whose
header names are given so the integrator can confirm them against the
downloaded distribution's `include/` directory. Statements about the Rust
`cef` crate are limited to what the saved docs.rs page shows (see
"What is known about the `cef` crate"). Anything else is marked *verify*.

## 1. What the crate provides (default build)

| module | purpose | key tests |
| --- | --- | --- |
| `url` | strict `http`/`https` normalisation (`NormalizedUrl`), relative resolution, percent-coding, `host_in_domain` | `url::tests::*` (13 tests) |
| `doi` | DOI scanning in text/URLs/HTML, arXiv ids in URLs/text, resolver hosts | `doi::tests::*` (8) |
| `hosts` | `SCHOLARLY_HOSTS` allowlist, `is_scholarly_host`, `ResearchPolicy::decide`, `LibraryProxy` (`EZproxy` host unwrapping, login URL) | `hosts::tests::*` (6) |
| `pdf` | `PdfVerdict` classification from URL and from `Content-Type`/`Content-Disposition`, `looks_like_pdf_bytes`, PDF links on a page | `pdf::tests::*` (10) |
| `cookies` | `Cookie` (redacting `Debug`), `Set-Cookie` parsing with HTTP dates, request matching, Netscape `cookies.txt`, `CookieJar`, credential-store bridge | `cookies::tests::*` (12) |
| `session` | `BrowserSession`: navigation decisions, response interception, cookies, page inspection | `session::tests::*` (9) |
| `bundle` | expected macOS bundle layout and signing order | `bundle::tests::*` (2) |
| `html` | tolerant tag/attribute/entity scanner behind `doi` and `pdf` | `html::tests::*` (7) |

Public API (re-exported at the crate root): `BrowserSession`, `Navigation`,
`Intercept`, `PageFacts`, `NormalizedUrl`, `ResearchPolicy`, `HostDecision`,
`LibraryProxy`, `is_scholarly_host`, `PdfVerdict`, `LinkClassification`,
`ResponseHints`, `Cookie`, `CookieJar`, `CookieSecretStore`,
`MemoryCookieStore`, `BrowserError`.

### The session contract

`BrowserSession::navigate(url)` returns one of:

- `Navigation::Blocked { url, host }`: research mode refuses the host
  (`session::tests::research_mode_blocks_and_open_mode_loads`).
- `Navigation::Load { url, dois, proxied_origin }`: load normally; `dois`
  are DOIs visible in the URL (`landing_pages_load_with_their_dois`),
  `proxied_origin` is the publisher behind a library proxy
  (`proxied_hosts_are_judged_by_their_origin`).
- `Navigation::Handoff { url, intercept }`: hand a DOI, an arXiv id or a PDF
  URL to the engine (`doi_resolver_hands_off_the_doi`,
  `arxiv_and_pdf_urls_hand_off`). A handoff never blocks; whether the shell
  also displays the page is the app's decision. For PDFs it should not, so
  that Chromium's PDF viewer does not consume the bytes the engine wants.

`on_response(url, hints)` re-judges with headers; `text/html` where a PDF was
expected yields `PdfVerdict::No` with the reason "login wall, interstitial or
landing page" (`responses_hand_off_pdfs_but_not_login_walls`,
`pdf::tests::response_media_type_confirms_or_refutes`). Final confirmation is
always `looks_like_pdf_bytes` on the first bytes (`pdf::tests::pdf_magic`).

`inspect_html(url, html)` extracts the page's own DOI (`citation_doi` and
friends), every DOI mentioned, arXiv ids and PDF candidates
(`inspecting_a_page_collects_facts`).

## 2. CEF on macOS

CEF (Chromium Embedded Framework) is a multi-process runtime. On macOS the
distribution is a **framework bundle** plus **helper applications**; the main
executable never links `libcef` directly but loads it at run time.

### 2.1 Bundle layout

`bundle::MacBundleLayout::new("TPE", "org.example.tpe")` computes the layout
below (`bundle::tests::layout_paths`):

```text
TPE.app/
  Contents/
    Info.plist
    MacOS/TPE                                   main (browser) process
    Resources/                                  icons, nibs, locale packs (verify: CEF's .pak files
                                                live inside the framework's Resources, not here)
    Frameworks/
      Chromium Embedded Framework.framework/    the CEF binary, Libraries/, Resources/
      TPE Helper.app                            generic helper
      TPE Helper (Alerts).app                   alert/notification helper
      TPE Helper (GPU).app                      GPU process
      TPE Helper (Plugin).app                   plugin process
      TPE Helper (Renderer).app                 renderer processes
```

The helper suffix list mirrors `CEF_HELPER_APP_SUFFIXES` in CEF's
`cmake/cef_macros.cmake` (`bundle::HELPER_SUFFIXES`). *Verify* against the
distribution's `tests/cefsimple/mac` and `cmake/` directory: the set of helper
types has changed between CEF majors, and the `cef` crate's own bundling
helper (if it has one) may generate them for you.

Each helper is a tiny app whose `main` calls the CEF process entry point and
exits. Helper bundles need their own `Info.plist` with `LSUIElement = true`
(no Dock icon) and a `CFBundleIdentifier` derived from the main bundle
(`HelperApp::bundle_id`, e.g. `org.example.tpe.helper.gpu`). Chromium picks
the helper by appending the suffix to the main executable name, which is why
the names must match exactly.

### 2.2 Loading the framework

On macOS the framework is loaded dynamically. In the C++ SDK this is
`CefScopedLibraryLoader` (`include/wrapper/cef_library_loader.h`), which
calls the C function `cef_load_library(path)`; the path is
`<bundle>/Contents/Frameworks/Chromium Embedded Framework.framework/Chromium Embedded Framework`
(`MacBundleLayout::cef_library`). Helpers load it relative to their own
location (`../../../../Chromium Embedded Framework.framework/...`). In Rust
this corresponds to whatever `cef`/`cef-dll-sys` expose for library loading
(the crate depends on `libloading`, consistent with dynamic loading). TODO:
confirm the exact function in the `cef` 154 docs.

### 2.3 Process start-up

1. Every process (main and helpers) first calls the CEF process entry point
   (C API `cef_execute_process`). It returns `-1` in the browser process and
   the exit code in helper processes.
2. The browser process then calls `cef_initialize` with settings: the
   `cache_path` (session persistence; see cookies), `browser_subprocess_path`
   (unused on macOS when helper bundles exist), `framework_dir_path`,
   `main_bundle_path`, `log_file`, and `no_sandbox` if the sandbox is not wired.
3. `cef_run_message_loop` (or integration with the app's own run loop via
   `external_message_pump` + `cef_do_message_loop_work`), then `cef_shutdown`.

`tpe_browser::embed::initialize` / `run` (feature `cef`) are the reserved
entry points for steps 2–3; they return `BrowserError::Unavailable` today.

### 2.4 Sandbox

The macOS distribution ships `libcef_sandbox.a`; helper processes link it and
call `cef_sandbox_initialize(argc, argv)` before anything else so the renderer
and GPU processes run in the Chromium sandbox (`include/cef_sandbox_mac.h`).
*Verify* how the `cef` crate links this static library; if it does not, run
with `no_sandbox` during the spike and document the security downgrade.

### 2.5 Code signing and notarisation

Every nested bundle carries its own signature and the outer signature seals
the inner ones, so signing is **inside-out**
(`MacBundleLayout::signing_order`, `bundle::tests::signing_is_inside_out`):

1. `Chromium Embedded Framework.framework` (its `Libraries/*.dylib` are sealed with it)
2. each `TPE Helper*.app`, with hardened runtime and the helper entitlements
   in `bundle::HELPER_ENTITLEMENTS`:
   `com.apple.security.cs.allow-jit`,
   `com.apple.security.cs.allow-unsigned-executable-memory`,
   `com.apple.security.cs.disable-library-validation`
3. `TPE.app` with the app entitlements (`com.apple.security.network.client`,
   `com.apple.security.files.user-selected.read-write` when saving PDFs
   through the panel)

```sh
codesign --force --options runtime --timestamp --sign "$IDENTITY" \
  "TPE.app/Contents/Frameworks/Chromium Embedded Framework.framework"
for h in "TPE.app/Contents/Frameworks/TPE Helper"*.app; do
  codesign --force --options runtime --timestamp --entitlements helper.entitlements \
    --sign "$IDENTITY" "$h"
done
codesign --force --options runtime --timestamp --entitlements app.entitlements \
  --sign "$IDENTITY" "TPE.app"
xcrun notarytool submit TPE.zip --keychain-profile tpe --wait && xcrun stapler staple TPE.app
```

The entitlement names are the ones Chromium-based apps (CEF samples,
Electron) use for their helper processes. *Verify* that the current CEF
release does not need additional ones (`codesign -d --entitlements - <helper>`
on the `cefclient` sample from the same distribution is the reference).

### 2.6 What is known about the `cef` crate

From the saved docs.rs page for `cef` 154.2.0+154.0.28 (2026-09-27),
repository `tauri-apps/cef-rs`:

- description: "Use the Chromium Embedded Framework in Rust";
- required dependencies: `cef-dll-sys ^154.2.0`, `libloading ^0.9`, `objc2 ^0.6.3`;
- optional dependencies include `objc2-foundation`, `objc2-io-surface`,
  `objc2-metal`, `plist`, `wgpu`, `ash`, `serde`, `clap`, `tracing`, `windows`;
- source size 51.7 MB; documentation coverage 95.5 %.

Nothing about the API surface, build script, environment variables or how the
CEF binary distribution is obtained is available offline, so **no calls into
the crate are made**. `crates/tpe-browser/Cargo.toml` declares
`cef = { version = "154", optional = true }` behind the `cef` feature (off by
default). The optional dependency is resolved into `Cargo.lock` but neither
downloaded nor compiled unless the feature is enabled. TODO for the
integrator: read the crate's README/build docs, confirm how the distribution
is located (env var vs. download), and fill in `embed::initialize`/`run`.

## 3. Sessions and cookies

### 3.1 Storage contract with `tpe-credentials`

Cookies persist through the credential store, never in plaintext files:

- service `cookies` (`cookies::COOKIE_SERVICE`), account = storage host
  (domain without its leading dot), secret = JSON array of
  `Cookie { name, value, domain, path, expires_unix, secure, http_only }`
  (`cookies::tests::store_round_trip_uses_contract_layout` asserts the JSON
  field names).
- `CookieSecretStore` mirrors `tpe_credentials::CredentialStore`
  method-for-method (`get`, `set`, `delete`, `list`) with `String` secrets.
  `tpe-browser` cannot depend on the `tpe-credentials` path while the two
  crates land in separate PRs (Cargo refuses a workspace whose path
  dependency is missing, even when optional), so the real wiring is one
  adapter written once both are on `main`:

```text
impl CookieSecretStore for CredentialAdapter<S: CredentialStore> {
    get    -> self.0.get(service, account)?.map(|s| <expose Secret as String>)
    set    -> self.0.set(service, account, &Secret::from(secret))
    delete -> self.0.delete(service, account)
    list   -> self.0.list(service)
}
```

`CookieJar::export_to_store` / `import_from_store` then work unchanged.
`tpe-credentials`'s own `CookieJarStore::cookie_header(host)` and
`CookieJar::header_for(url, now)` compute the same thing from the same
records; both drop expired cookies
(`cookies::tests::header_orders_longer_paths_first_and_drops_expired`).

### 3.2 Injecting into CEF and reading back

CEF keeps cookies in its own store under `cache_path` (persistent across runs
when `persist_session_cookies` is set in the request-context settings). The
credential store is the *source of truth* between runs and across the
engine's own HTTP client; CEF's store is a cache we fill and drain:

- **Start-up**: `CookieJar::import_from_store` → for each cookie call the
  cookie manager's `set_cookie(url, cookie, callback)` (C API
  `cef_cookie_manager_t::set_cookie`; the global manager comes from
  `cef_cookie_manager_get_global_manager(callback)`; header
  `include/capi/cef_cookie_capi.h`). The `url` argument must be an
  `https://<storage host>/` URL for the cookie to be accepted; `secure`
  cookies require an `https` URL. Field mapping: `name`, `value`, `domain`
  (leading dot preserved), `path`, `secure`, `httponly`, `has_expires` +
  `expires` (from `expires_unix`; session cookies have `has_expires = 0`).
- **During browsing**: nothing to do; Chromium manages the jar. Optionally
  `CefResponse` headers can be mirrored through
  `BrowserSession::on_set_cookie` so the model's jar stays a faithful copy
  (`session::tests::cookies_flow_through_the_session`).
- **Shutdown / after login**: `visit_all_cookies(visitor)` → build a
  `CookieJar` → `export_to_store`; `flush_store(callback)` before quitting.
  Only hosts under the research policy or the proxy are exported; other
  cookies stay in CEF's cache.

Never log cookie values: `Cookie`'s `Debug` prints `value: "***"`
(`cookies::tests::debug_redacts_value`), and `MemoryCookieStore` redacts too.

### 3.3 Netscape `cookies.txt`

`CookieJar::to_netscape` / `parse_netscape` read and write the seven-field
format used by curl, `wget`, browser extensions and CEF's `cefclient`
(`cookies::tests::netscape_round_trip`). `HttpOnly` cookies use the curl
`#HttpOnly_` domain prefix. This is the hand-off format for importing a
librarian-provided session or for feeding `tpe-biblio`'s client.

## 4. Request interception for DOI and PDF links

All hooks are on the browser process's `CefClient` handlers (C++ names;
the C API mirrors them as `cef_request_handler_t`,
`cef_resource_request_handler_t`, `cef_download_handler_t`,
`cef_load_handler_t`):

| CEF hook | model call | effect |
| --- | --- | --- |
| `CefRequestHandler::OnBeforeBrowse(browser, frame, request, user_gesture, is_redirect)` | `BrowserSession::navigate(request.url)` | `Blocked` → return `true` (cancel) and show the notice; `Handoff { Doi }` → queue the DOI for `tpe-biblio` and let the navigation continue; `Handoff { Arxiv }` → queue `arxiv.org/pdf/<id>` for the engine; `Handoff { Pdf }` → cancel the navigation and start a download instead (`CefBrowserHost::StartDownload(url)`) |
| `CefResourceRequestHandler::OnResourceResponse(browser, frame, request, response)` (main frame only) | `on_response(url, ResponseHints { content_type: response.GetMimeType(), content_disposition: response.GetHeaderByName("Content-Disposition") })` | `Some(Intercept::Pdf)` → cancel rendering and download; `None` with reason "login wall" while a PDF was expected → surface "sign in to the library proxy" |
| `CefDownloadHandler::OnBeforeDownload(browser, item, suggested_name, callback)` / `OnDownloadUpdated` | `pdf::parse_content_disposition` gives the sanitised filename (`pdf::tests::content_disposition_parsing`) | save under the corpus inbox, then `looks_like_pdf_bytes` on the first 1 KiB before calling `tpe extract` |
| `CefLoadHandler::OnLoadEnd(browser, frame, status)` + `frame.GetSource(visitor)` | `inspect_html(url, html)` | record `PageFacts`; offer "Extract PDF" for each `pdf_links` entry; resolve `primary_doi` metadata |

The classifier's confidence ladder is `No < Possible < Likely < Confirmed`
(`pdf::tests::verdict_order_and_labels`); only `Likely` or better is handed
off (`session::pdf_intercept`). Opaque media types
(`application/octet-stream`) are confirmed only with a `.pdf` filename or a
PDF-shaped URL (`pdf::tests::opaque_media_type_defers_to_filename_and_url`).

Chromium's built-in PDF viewer would otherwise render `application/pdf`
responses in-page. Cancelling in `OnResourceResponse` and re-requesting as a
download keeps one copy of the bytes, fetched with the session's cookies.
TODO: confirm in the `cef` 154 API whether a resource response can be
cancelled at that point or whether `GetResourceResponseFilter` must be used.

## 5. Research mode

`ResearchPolicy::research()` allows only `hosts::SCHOLARLY_HOSTS` (identifier
resolvers, indexes, preprint servers, repositories, publishers and library
platforms; `hosts::tests::allowlist_matches_suffixes_only`). Rules:

- matching is by domain suffix, so `pubmed.ncbi.nlm.nih.gov` matches
  `ncbi.nlm.nih.gov` and `nature.com.evil.example` matches nothing;
- `deny` beats everything, even in open mode (`extra_allow_and_deny`);
- `extra_allow` adds institutional SSO hosts and departmental servers; login
  domains of identity providers are *not* on the list on purpose: the user
  adds their institution's, or the library proxy covers it;
- `ResearchPolicy::open()` disables the allowlist but keeps proxy handling
  and the deny list;
- Google Scholar (`scholar.google.com`) is allowed for *browsing*; its terms
  forbid scraping and `tpe-biblio` only builds search URLs for it. The
  `casa_token` parameter that grants campus access is preserved by URL
  normalisation (`url::tests::tracking_params_are_removed_but_order_kept`),
  while `utm_*`, `fbclid`, `gclid` and similar are stripped.

Non-web schemes (`javascript:`, `data:`, `file:`, `chrome:`) are refused at
parse time (`url::tests::non_web_schemes_are_refused`), and credentials in a
URL are dropped before anything is stored or logged
(`credentials_are_dropped`).

## 6. Publisher and library proxy sessions

Institutions expose paywalled content through a proxy. Two shapes are modelled
(`hosts::ProxyKind`):

- **`EZproxy` by hostname**: `www.nature.com` is served as
  `www-nature-com.ezproxy.lib.edu` (dots to hyphens, one wildcard
  certificate) or, on older setups, `www.nature.com.ezproxy.lib.edu`. A
  session starts at `https://ezproxy.lib.edu/login?url=<target>`
  (`LibraryProxy::proxied_url`, `hosts::tests::proxied_login_url_encodes_target`).
  `LibraryProxy::unproxy_host` maps the rewritten host back
  (`ezproxy_hosts_are_unwrapped`); a publisher host containing a real hyphen
  is ambiguous under the hyphen form, which is documented on the method.
- **Redirector** (`OpenAthens`-style): only the entry URL is rewritten; hosts
  are not.

Policy decisions for a proxied host are made on the *origin*:
`www-nature-com.ezproxy.lib.edu` yields
`HostDecision::Proxied { origin: "www.nature.com" }`, the proxy's own
login pages are always allowed, and `www-example-com.ezproxy.lib.edu` is
blocked in research mode (`hosts::tests::ezproxy_hosts_are_unwrapped`,
`session::tests::proxied_hosts_are_judged_by_their_origin`). A proxied
`doi-org` host still hands off the DOI.

Session cookies set by the proxy carry `Domain=.ezproxy.lib.edu`, so a single
login covers every proxied publisher; `CookieJar::header_for` returns the
proxy cookie for any `https://*-*.ezproxy.lib.edu/` request and nothing for
plain `http` (the cookie is `Secure`) or for the un-proxied publisher
(`session::tests::cookies_flow_through_the_session`). The engine's HTTP
client (`tpe-biblio`) should therefore fetch `requires_session: true`
candidates *through the proxy URL* with the exported cookies, or leave them
to the shell. When the proxy session expires the publisher answers a PDF
request with an HTML login page; the classifier reports it (section 4) and
the app prompts for re-login rather than storing HTML as a PDF.

## 7. Security and privacy notes

- No network in this crate; no secrets in code, logs or test fixtures.
- Cookie values are redacted from `Debug`; URLs lose `user:pass@`.
- The credential store, not CEF's `cache_path`, is the durable cookie store;
  CEF's cache directory should live under the app's sandboxed container.
- Research mode is an allowlist, so unknown trackers, CDNs and ad hosts
  embedded in publisher pages are also refused as sub-resources if the same
  policy is applied in `OnBeforeResourceLoad` (recommended; the model does
  not distinguish navigations from sub-resources).

## 8. Verification checklist for the integrator (macOS, with the CEF distribution)

1. `ls "Chromium Embedded Framework.framework"` and `tests/cefsimple/mac` in
   the distribution: confirm the helper suffix set against
   `bundle::HELPER_SUFFIXES`.
2. `include/capi/cef_cookie_capi.h`: confirm `set_cookie`, `visit_all_cookies`,
   `flush_store`, `cef_cookie_manager_get_global_manager` and the
   `cef_cookie_t` fields named in section 3.2.
3. `include/capi/cef_request_handler_capi.h`,
   `cef_resource_request_handler_capi.h`, `cef_download_handler_capi.h`:
   confirm the hooks in section 4 and where a response can be cancelled.
4. `cef` crate docs: find the Rust spellings of `cef_execute_process`,
   `cef_initialize`, `cef_run_message_loop`, `cef_shutdown`, library loading,
   and how `cef-dll-sys` locates the distribution; then implement
   `embed::initialize` / `embed::run`.
5. Sign a debug build inside-out (section 2.5) and check
   `codesign --verify --deep --strict TPE.app` plus `spctl --assess`.
6. Log in to the institution's proxy in the shell, quit, relaunch: the proxy
   cookie must come back from the credential store and a proxied PDF must
   download without a second login.
