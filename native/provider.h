/* Optional native PDF provider ABI v1. This header has no engine dependency.
 * Provider/runtime code is trusted deployment code, never loaded from search paths.
 * Calls are sequential per handle. Distinct handles must be independent.
 * No exception, longjmp, or panic may cross an exported function boundary.
 * Input remains immutable and alive until close. No subprocesses are permitted.
 * Output is UTF-8 JSON, allocated by provider, freed exactly once with free.
 * page must enforce output_limit during construction, not just after allocation.
 * Every return initializes outputs; failures leave NULL/zero outputs.
 * Error strings are NUL terminated within error_capacity. Static string exports
 * are NUL terminated, at most 255 bytes. See README.md for the JSON contract. */
#ifndef TPE_PDF_PROVIDER_H
#define TPE_PDF_PROVIDER_H
#include <stddef.h>
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
uint32_t tpe_pdf_provider_abi_version(void);
const char *tpe_pdf_provider_engine(void);
const char *tpe_pdf_provider_version(void);
const char *tpe_pdf_provider_runtime_symbol(void);
uintptr_t tpe_pdf_provider_runtime_anchor(void);
int tpe_pdf_provider_open(const unsigned char *, size_t, const char *, void **,
    uint32_t *, char *, size_t);
int tpe_pdf_provider_page(void *, uint32_t, size_t, unsigned char **, size_t *, char *, size_t);
void tpe_pdf_provider_free(unsigned char *);
void tpe_pdf_provider_close(void *);
#ifdef __cplusplus
}
#endif
#endif
