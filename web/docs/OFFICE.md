# Browser Office extraction

`web/lib/office.ts` extracts Office packages locally in the browser. It does not
send the document to an Office conversion service or execute macros, formulas,
embedded objects, or linked images.

```ts
const kind = await detectOffice(file, signal);
const { extracted, assets } = await extractOffice(file, signal);
// Or store each image as it becomes available, without retaining all images:
const result = await extractOffice(file, signal, async ({ id, file }) => {
  await storeAsset(id, file);
});
```

With the async callback, `assets` is empty; the callback receives the same image
IDs used in `<img data-image-id="office-image-1">`. The caller replaces these
IDs with its own private stored asset URLs. Referenced images retain their
original decoded bytes and MIME type. Unsafe SVG content is omitted with a
warning. External images are not fetched. Failed asset storage produces an
explicit partial extraction and removes the corresponding image markup.

| Format | Implemented evidence | Remaining limitations |
| --- | --- | --- |
| DOCX | Source paragraph order, headings, bold/italic, text breaks/tabs, tables, hyperlinks, embedded images, header/footer and note text | Not a page renderer; floating placement, automatic list labels, vertical table merges, field evaluation, and embedded objects are not reconstructed. Headers/footers/notes appear after the body. |
| PPTX | Presentation relationship order, slide text and titles, shape order, tables, hyperlinks, images, speaker notes, slide source parts | Not a slide renderer; layouts/masters, charts, SmartArt, visual overlap, animations, audio/video, and embedded objects are not interpreted. |
| XLSX | Workbook sheet order, shared/rich/inline strings, cell addresses, stored numeric/boolean values, cached formulas plus formula source, hyperlinks, merge ranges, drawing images | No recalculation, style/number/date formatting, charts, or grid layout reproduction. Sparse cells are explicitly labeled by address rather than allocating missing rows/columns. |
| ODF | Available `content.xml` text and images | Always partial; no claim of full ODF layout/formula support. |
| Pages/Keynote/Numbers | Available legacy XML text and QuickLook image previews | Always partial. Binary `.iwa` document structures are not decoded. A preview is not extracted body text. |
| Legacy/encrypted binary Office | Explicit unsupported result | `.doc`, `.ppt`, `.xls`, and OLE-encrypted packages need a separate reader. |

`ready` means the supported document projection completed without observed
warnings; it is not a pixel-accurate rendering or a claim that every Office
feature is implemented. Detected missing relationships, unsupported content,
malformed XML, inaccessible images, and partial-format handling set `partial`.
XML declarations defining DTDs/entities are rejected. Archive paths cannot
escape their package. HTML text and attributes are escaped, and hyperlink
schemes are restricted to HTTP, HTTPS, mailto, and local fragments.

Detection reads only the ZIP end record and central directory using `File.slice`
and supports ZIP64 directory offsets. Extraction uses `File.stream()` with
fflate's streaming `Unzip`. It starts a selective decoder for every member,
because simply leaving an fflate member unstarted would retain its compressed
bytes. Unneeded binary entries are discarded without decompression. The XML
pass discovers referenced images; a second streaming pass decodes only those
images. The reader awaits image-storage callbacks before reading further input.

There are no application file-size, text-size, entry-count, or image-size caps.
XML strings, parsed DOMs, extracted text/HTML, and one or more images from the
current stream chunk still require browser resources. The default return-array
mode retains all extracted image Files; use the callback for larger documents.
This implementation does not guarantee processing a 100 GB Office document in
browser memory. ZIP checksums are not verified by fflate's streaming reader;
this module is not an archive-integrity verifier. It does not promise visual
reading order for objects whose stored order differs from their displayed order.

Run the focused fixtures from `web/`:

```sh
node scripts/test-office.mjs
node node_modules/typescript/bin/tsc --noEmit --pretty false
```

The fixtures create actual ZIP packages and verify DOCX/PPTX/XLSX text, structure,
relationships, order, tables, exact embedded PNG bytes, selective binary skipping,
async image delivery, malformed XML, unsafe links, partial ODF/iWork evidence,
and cancellation. They prohibit whole-file `arrayBuffer()` reads. They are
contract fixtures, not a corpus benchmark or an end-to-end storage/UI test.

Primary format references:

- [WordprocessingML document structure](https://learn.microsoft.com/en-us/office/open-xml/word/structure-of-a-wordprocessingml-document)
- [Spreadsheet shared strings](https://learn.microsoft.com/en-us/office/open-xml/spreadsheet/working-with-the-shared-string-table)
- [Spreadsheet structure](https://learn.microsoft.com/en-us/office/open-xml/spreadsheet/structure-of-a-spreadsheetml-document)
