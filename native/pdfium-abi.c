// Public C API sentinel for the LP64 Linux worker, compiled against its source
// checkout and linked to the very library exercised by the Rust tests.
#include <assert.h>
#include <math.h>
#include <stddef.h>
#include <stdio.h>

#include "fpdf_edit.h"
#include "fpdf_text.h"
#include "fpdfview.h"

_Static_assert(sizeof(void *) == 8, "this sentinel verifies 64-bit Linux");
_Static_assert(sizeof(unsigned long) == sizeof(size_t), "binding requires LP64");
_Static_assert(sizeof(int) == 4, "binding uses 32-bit native counts");
_Static_assert(sizeof(double) == 8, "geometry output pointers target doubles");
_Static_assert(_Generic(&FPDF_LoadMemDocument64,
    FPDF_DOCUMENT (*)(const void *, size_t, FPDF_BYTESTRING): 1, default: 0),
    "memory-document ABI changed");
_Static_assert(_Generic(&FPDFText_GetCharBox,
    FPDF_BOOL (*)(FPDF_TEXTPAGE, int, double *, double *, double *, double *): 1,
    default: 0), "character-box ABI changed");
_Static_assert(_Generic(&FPDFText_GetCharOrigin,
    FPDF_BOOL (*)(FPDF_TEXTPAGE, int, double *, double *): 1, default: 0),
    "character-origin ABI changed");

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: pdfium-abi nested-forms.pdf\n");
    return 2;
  }
  FPDF_InitLibrary();
  FPDF_DOCUMENT document = FPDF_LoadDocument(argv[1], NULL);
  assert(document != NULL);
  assert(FPDF_GetPageCount(document) == 2);
  for (int index = 0; index < 2; ++index) {
    FPDF_PAGE page = FPDF_LoadPage(document, index);
    assert(page != NULL);
    assert(isfinite(FPDF_GetPageWidthF(page)));
    assert(isfinite(FPDF_GetPageHeightF(page)));
    assert(FPDFPage_GetRotation(page) == index);
    FPDF_TEXTPAGE text = FPDFText_LoadPage(page);
    assert(text != NULL);
    int count = FPDFText_CountChars(text);
    assert(count >= 24);
    int occurrences = 0;
    for (int character = 0; character < count; ++character) {
      if (FPDFText_GetUnicode(text, character) != 'N') {
        continue;
      }
      double left, right, bottom, top, x, y;
      assert(FPDFText_GetCharBox(text, character, &left, &right, &bottom, &top));
      assert(FPDFText_GetCharOrigin(text, character, &x, &y));
      assert(isfinite(left) && isfinite(right) && isfinite(bottom) && isfinite(top));
      assert(isfinite(x) && isfinite(y));
      assert(fabs(FPDFText_GetFontSize(text, character) - 12.0) < 0.01);
      assert(isfinite(FPDFText_GetCharAngle(text, character)));
      int generated = FPDFText_IsGenerated(text, character);
      int hyphen = FPDFText_IsHyphen(text, character);
      assert(generated == 0 || generated == 1);
      assert(hyphen == 0 || hyphen == 1);
      ++occurrences;
    }
    assert(occurrences == 4);
    FPDFText_ClosePage(text);
    FPDF_ClosePage(page);
  }
  FPDF_CloseDocument(document);
  FPDF_DestroyLibrary();
  puts("public C ABI and two-page character geometry passed (Linux LP64)");
  return 0;
}
