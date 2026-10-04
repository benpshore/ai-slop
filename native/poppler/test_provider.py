"""Real-runtime ABI tests; no external PDF fixture or Python package needed."""

import ctypes
import json
import sys
import unittest
from pathlib import Path


def pdf(
    content=b"BT /F1 12 Tf 20 40 Td (Visible citation) Tj ET",
    rotate=0,
    crop=None,
    uri=b"https://doi.org/10.1234/raw?x=1&y=2",
    base=b"",
    internal_link=False,
    link_rect=b"/Rect [20 30 100 45]",
):
    """Build an exact xref fixture with a URI distinct from the visible text."""
    crop_box = f" /CropBox [{crop}]" if crop else ""
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R /URI << /Base (" + base + b") >> >>",
        b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>",
        (
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100]"
            f" /Rotate {rotate}{crop_box} /Resources << /Font << /F1 4 0 R >> >>"
            f" /Contents 5 0 R /Annots [6 0 R{' 7 0 R' if internal_link else ''}] >>"
        ).encode(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        b"<< /Length " + str(len(content)).encode() + b" >>\nstream\n" + content + b"\nendstream",
        b"<< /Type /Annot /Subtype /Link "
        + link_rect
        + b" /A << /S /URI /URI ("
        + uri
        + b") >> >>",
        b"<< /Type /Annot /Subtype /Link /Rect [0 0 10 10] /A << /S /GoTo /D [3 0 R /Fit] >> >>"
        if internal_link
        else b"null",
    ]
    output = bytearray(b"%PDF-1.7\n")
    offsets = [0]
    for number, obj in enumerate(objects, 1):
        offsets.append(len(output))
        output.extend(f"{number} 0 obj\n".encode() + obj + b"\nendobj\n")
    xref = len(output)
    output.extend(f"xref\n0 {len(offsets)}\n0000000000 65535 f \n".encode())
    for offset in offsets[1:]:
        output.extend(f"{offset:010d} 00000 n \n".encode())
    output.extend(
        f"trailer\n<< /Size {len(offsets)} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    )
    return bytes(output)


class Provider:
    def __init__(self, provider, runtime):
        self.runtime = ctypes.CDLL(runtime, mode=ctypes.RTLD_GLOBAL)
        self.lib = ctypes.CDLL(provider)
        self.lib.tpe_pdf_provider_runtime_anchor.restype = ctypes.c_size_t
        for suffix in ("engine", "version", "runtime_symbol"):
            getattr(self.lib, "tpe_pdf_provider_" + suffix).restype = ctypes.c_char_p
        self.lib.tpe_pdf_provider_open.argtypes = [
            ctypes.c_void_p,
            ctypes.c_size_t,
            ctypes.c_char_p,
            ctypes.POINTER(ctypes.c_void_p),
            ctypes.POINTER(ctypes.c_uint32),
            ctypes.c_void_p,
            ctypes.c_size_t,
        ]
        self.lib.tpe_pdf_provider_page.argtypes = [
            ctypes.c_void_p,
            ctypes.c_uint32,
            ctypes.c_size_t,
            ctypes.POINTER(ctypes.c_void_p),
            ctypes.POINTER(ctypes.c_size_t),
            ctypes.c_void_p,
            ctypes.c_size_t,
        ]
        self.lib.tpe_pdf_provider_close.argtypes = [ctypes.c_void_p]
        self.lib.tpe_pdf_provider_free.argtypes = [ctypes.c_void_p]

    def open(self, data, password=None):
        source = ctypes.create_string_buffer(data)
        handle, pages, error = (
            ctypes.c_void_p(),
            ctypes.c_uint32(),
            ctypes.create_string_buffer(256),
        )
        result = self.lib.tpe_pdf_provider_open(
            source, len(data), password, ctypes.byref(handle), ctypes.byref(pages), error, 256
        )
        return result, handle, pages.value, error.value, source

    def page(self, handle, number=1, limit=16 * 1024 * 1024):
        data, length, error = (
            ctypes.c_void_p(123),
            ctypes.c_size_t(999),
            ctypes.create_string_buffer(256),
        )
        result = self.lib.tpe_pdf_provider_page(
            handle, number, limit, ctypes.byref(data), ctypes.byref(length), error, 256
        )
        if result:
            if data.value or length.value:
                raise AssertionError("failure leaked output ownership")
            return result, None, error.value
        try:
            if length.value > limit:
                raise AssertionError("provider exceeded its output limit")
            return result, json.loads(ctypes.string_at(data, length.value)), error.value
        finally:
            self.lib.tpe_pdf_provider_free(data)


PROVIDER = Provider(sys.argv.pop(1), sys.argv.pop(1))


class ContractTests(unittest.TestCase):
    def extract(self, source):
        status, handle, count, error, retained = PROVIDER.open(source)
        self.assertEqual((status, count), (0, 1), error)
        try:
            status, page, error = PROVIDER.page(handle)
            self.assertEqual(status, 0, error)
            self.assertEqual(retained.raw[:-1], source)
            return page
        finally:
            PROVIDER.lib.tpe_pdf_provider_close(handle)

    def test_exact_engine_and_runtime_identity(self):
        lib = PROVIDER.lib
        self.assertEqual(lib.tpe_pdf_provider_abi_version(), 1)
        self.assertEqual(lib.tpe_pdf_provider_engine(), b"poppler")
        self.assertEqual(lib.tpe_pdf_provider_version(), b"26.09.0")
        symbol = lib.tpe_pdf_provider_runtime_symbol().decode()
        anchor = ctypes.addressof(ctypes.c_void_p.in_dll(PROVIDER.runtime, symbol))
        self.assertEqual(lib.tpe_pdf_provider_runtime_anchor(), anchor)

    def test_geometry_fonts_and_separate_raw_uri(self):
        page = self.extract(pdf())
        self.assertEqual(page["bounds"], [0, 0, 200, 100])
        runs = [line for block in page["structured"]["blocks"] for line in block["lines"]]
        self.assertEqual("".join(run["text"] for run in runs), "Visible citation")
        self.assertEqual(page["characters"], len("Visible citation"))
        self.assertEqual((page["warnings"], page["unmapped"]), (0, 0))
        self.assertTrue(all(run["font"]["name"] == "Helvetica" for run in runs))
        self.assertTrue(all(run["font"]["size"] == 12 for run in runs))
        self.assertTrue(all(run["bbox"]["w"] > 0 and run["bbox"]["h"] > 0 for run in runs))
        self.assertEqual(
            page["links"],
            [
                {
                    "uri": "https://doi.org/10.1234/raw?x=1&y=2",
                    "bounds": [20, 55, 100, 70],
                }
            ],
        )

    def test_rotated_and_cropped_link_geometry(self):
        page = self.extract(pdf(rotate=90, crop="10 20 190 90"))
        self.assertEqual(page["bounds"], [0, 0, 70, 180])
        self.assertEqual(page["links"][0]["bounds"], [10, 10, 25, 90])
        self.assertEqual(page["page_size"], [180, 70])
        self.assertEqual(page["rotation"], 90)
        a, b, c, d, e, f = page["to_pdf"]
        x0, y0, x1, y1 = page["links"][0]["bounds"]
        corners = [(a * x + c * y + e, b * x + d * y + f) for x in (x0, x1) for y in (y0, y1)]
        self.assertEqual(
            [
                min(x for x, _ in corners),
                min(y for _, y in corners),
                max(x for x, _ in corners),
                max(y for _, y in corners),
            ],
            [20, 30, 100, 45],
        )

    def test_uri_is_not_resolved_against_base_or_rewritten(self):
        for raw in [b"../relative-doi", b"www.example.test/path"]:
            page = self.extract(pdf(uri=raw, base=b"https://example.test/base", internal_link=True))
            self.assertEqual(len(page["links"]), 1)
            self.assertEqual(page["links"][0]["uri"], raw.decode())

    def test_missing_link_rectangle_preserves_uri_without_fabricated_geometry(self):
        page = self.extract(pdf(link_rect=b""))
        self.assertEqual(len(page["links"]), 1)
        self.assertIsNone(page["links"][0]["bounds"])
        self.assertGreater(page["warnings"], 0)

    def test_output_limit_and_bad_page_leave_no_owned_output_then_recover(self):
        status, handle, _, error, retained = PROVIDER.open(pdf())
        self.assertEqual(status, 0, error)
        try:
            self.assertNotEqual(PROVIDER.page(handle, limit=32)[0], 0)
            self.assertNotEqual(PROVIDER.page(handle, number=0)[0], 0)
            self.assertNotEqual(PROVIDER.page(handle, number=2)[0], 0)
            self.assertEqual(PROVIDER.page(handle)[0], 0)
            self.assertTrue(retained.raw.startswith(b"%PDF"))
        finally:
            PROVIDER.lib.tpe_pdf_provider_close(handle)

    def test_parser_repair_cannot_claim_complete(self):
        page = self.extract(pdf(b"BT /F1 12 Tf 20 40 Td (Kept) Tj ET bogusoperator"))
        self.assertGreater(page["warnings"], 0)
        self.assertIn("Kept", str(page["structured"]))
        self.assertEqual(self.extract(pdf())["warnings"], 0)

    def test_invalid_open_and_blank_page(self):
        status, handle, count, _, retained = PROVIDER.open(b"not a PDF")
        self.assertNotEqual(status, 0)
        self.assertIsNone(handle.value)
        self.assertEqual(count, 0)
        self.assertEqual(retained.value, b"not a PDF")
        page = self.extract(pdf(b""))
        self.assertEqual(page["characters"], 0)
        self.assertEqual(page["warnings"], 0)

    def test_missing_mapping_preserves_replacement_evidence(self):
        fixture = Path(__file__).parents[2] / "tests/fixtures/pdfium-unicode/partial-cmap.pdf"
        page = self.extract(fixture.read_bytes())
        text = "".join(
            line["text"] for block in page["structured"]["blocks"] for line in block["lines"]
        )
        self.assertGreater(page["unmapped"], 0)
        self.assertGreater(page["warnings"], 0)
        self.assertIn("\ufffd", text)
        self.assertEqual(page["characters"], len(text))

    def test_password_required_wrong_and_correct(self):
        source = (Path(__file__).parent / "fixtures/password.pdf").read_bytes()
        for password, expected in [(None, 2), (b"wrong-test-password", 3)]:
            status, handle, count, _, retained = PROVIDER.open(source, password)
            self.assertEqual(status, expected)
            self.assertIsNone(handle.value)
            self.assertEqual(count, 0)
            self.assertEqual(retained.raw[:-1], source)
        status, handle, count, error, retained = PROVIDER.open(source, b"test-user")
        self.assertEqual((status, count), (0, 1), error)
        try:
            status, page, error = PROVIDER.page(handle)
            self.assertEqual(status, 0, error)
            self.assertEqual(page["warnings"], 0)
            self.assertEqual(page["links"][0]["uri"], "https://doi.org/10.1234/raw?x=1&y=2")
            self.assertEqual(retained.raw[:-1], source)
        finally:
            PROVIDER.lib.tpe_pdf_provider_close(handle)


if __name__ == "__main__":
    unittest.main()
