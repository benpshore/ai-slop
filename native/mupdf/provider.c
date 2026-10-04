/* Optional user-built provider. Never linked into, or shipped with, tpe.
 * MuPDF must be separately licensed by the operator; see README.md. */
#include <mupdf/fitz.h>
#include <mupdf/pdf.h>
#include "../provider.h"
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>

#if defined(_WIN32)
#define API __declspec(dllexport)
#else
#define API __attribute__((visibility("default")))
#endif

struct session {
    fz_context *ctx;
    fz_document *doc;
    unsigned warnings;
};
struct output_buffer { unsigned char *data; size_t len, cap, limit; };

static void warning(void *opaque, const char *message) {
    struct session *s = opaque;
    (void)message;
    if (s->warnings != UINT32_MAX) s->warnings++;
}
static void error_text(char *out, size_t capacity, const char *message) {
    if (capacity) snprintf(out, capacity, "%s", message ? message : "MuPDF error");
}
static void write_output(fz_context *ctx, void *opaque, const void *data, size_t len) {
    struct output_buffer *out = opaque;
    if (len > out->limit - out->len)
        fz_throw(ctx, FZ_ERROR_LIMIT, "structured text exceeds configured output limit");
    if (out->len + len > out->cap) {
        size_t cap = out->cap ? out->cap : 4096;
        while (cap < out->len + len) {
            if (cap > out->limit / 2) { cap = out->limit; break; }
            cap *= 2;
        }
        unsigned char *next = realloc(out->data, cap);
        if (!next) fz_throw(ctx, FZ_ERROR_LIMIT, "structured text allocation failed");
        out->data = next;
        out->cap = cap;
    }
    memcpy(out->data + out->len, data, len);
    out->len += len;
}

API uint32_t tpe_pdf_provider_abi_version(void) { return 1; }
API const char *tpe_pdf_provider_engine(void) { return "mupdf"; }
API const char *tpe_pdf_provider_runtime_symbol(void) { return "fz_new_context_imp"; }
API const char *tpe_pdf_provider_version(void) { return FZ_VERSION; }
/* Verifies that the explicitly fingerprinted runtime satisfies our linkage. */
API uintptr_t tpe_pdf_provider_runtime_anchor(void) { return (uintptr_t)&fz_new_context_imp; }
API void tpe_pdf_provider_close(void *opaque) {
    struct session *s = opaque;
    if (!s) return;
    fz_drop_document(s->ctx, s->doc);
    fz_drop_context(s->ctx);
    free(s);
}
API void tpe_pdf_provider_free(unsigned char *data) { free(data); }

/* Input memory must remain immutable and alive until close. No file is opened.
 * Return 2=password required, 3=wrong password, 1=other failure, 0=success. */
API int tpe_pdf_provider_open(const unsigned char *bytes, size_t len, const char *password,
    void **result, uint32_t *pages, char *error, size_t error_capacity) {
    struct session *s = calloc(1, sizeof(*s));
    fz_stream *stream = NULL;
    int status = 1;
    *result = NULL;
    *pages = 0;
    if (!s) { error_text(error, error_capacity, "provider allocation failed"); return 1; }
    s->ctx = fz_new_context(NULL, NULL, 8 * 1024 * 1024);
    if (!s->ctx) { free(s); error_text(error, error_capacity, "MuPDF context failed"); return 1; }
    fz_set_warning_callback(s->ctx, warning, s);
    fz_var(stream);
    fz_var(status);
    fz_try(s->ctx) {
        fz_register_document_handlers(s->ctx);
        stream = fz_open_memory(s->ctx, bytes, len);
        s->doc = fz_open_document_with_stream(s->ctx, "application/pdf", stream);
        if (fz_needs_password(s->ctx, s->doc)) {
            if (!password) { status = 2; fz_throw(s->ctx, FZ_ERROR_ARGUMENT, "password required"); }
            if (!fz_authenticate_password(s->ctx, s->doc, password)) {
                status = 3; fz_throw(s->ctx, FZ_ERROR_ARGUMENT, "wrong password");
            }
        }
        int count = fz_count_pages(s->ctx, s->doc);
        if (count <= 0) fz_throw(s->ctx, FZ_ERROR_FORMAT, "document has no pages");
        *pages = (uint32_t)count;
        status = 0;
    }
    fz_always(s->ctx) { fz_drop_stream(s->ctx, stream); }
    fz_catch(s->ctx) { error_text(error, error_capacity, fz_caught_message(s->ctx)); }
    if (status) { tpe_pdf_provider_close(s); return status; }
    *result = s;
    return 0;
}

static void json_string(fz_context *ctx, fz_output *out, const char *text, size_t length) {
    fz_write_byte(ctx, out, '"');
    for (const unsigned char *p = (const unsigned char *)text, *end = p + length; p < end; ++p) {
        if (*p == '"' || *p == '\\') fz_write_byte(ctx, out, '\\');
        if (*p < 32) fz_write_printf(ctx, out, "\\u%04x", (unsigned)*p);
        else fz_write_byte(ctx, out, *p);
    }
    fz_write_byte(ctx, out, '"');
}

/* Every exception remains in C, never unwinding across Rust frames. */
API int tpe_pdf_provider_page(void *opaque, uint32_t number, size_t output_limit,
    unsigned char **data, size_t *len, char *error, size_t error_capacity) {
    struct session *s = opaque;
    fz_page *page = NULL;
    fz_stext_page *text = NULL;
    fz_output *writer = NULL;
    struct output_buffer *out = calloc(1, sizeof(*out));
    int status = 1;
    *data = NULL;
    *len = 0;
    if (!out) { error_text(error, error_capacity, "output allocation failed"); return 1; }
    out->limit = output_limit;
    fz_var(page); fz_var(text); fz_var(writer); fz_var(status);
    fz_try(s->ctx) {
        if (!number || number > (uint32_t)fz_count_pages(s->ctx, s->doc))
            fz_throw(s->ctx, FZ_ERROR_ARGUMENT, "page out of range");
        page = fz_load_page(s->ctx, s->doc, (int)number - 1);
        fz_rect bounds = fz_bound_page(s->ctx, page);
        fz_stext_options options = {0};
        /* No CID/GID-as-Unicode fallback: unmapped glyphs must remain U+FFFD. */
        options.flags = FZ_STEXT_PRESERVE_SPANS | FZ_STEXT_PRESERVE_IMAGES |
            FZ_STEXT_PRESERVE_WHITESPACE;
        text = fz_new_stext_page_from_page(s->ctx, page, &options);
        size_t chars = 0, unmapped = 0;
        for (fz_stext_block *b = text->first_block; b; b = b->next) {
            if (b->type != FZ_STEXT_BLOCK_TEXT) continue;
            for (fz_stext_line *l = b->u.t.first_line; l; l = l->next) {
                for (fz_stext_char *c = l->first_char; c; c = c->next) {
                    if (++chars > 1000000) fz_throw(s->ctx, FZ_ERROR_LIMIT, "page character limit exceeded");
                    if (c->c <= 0 || c->c == 0xfffd || c->c > 0x10ffff ||
                        (c->c >= 0xd800 && c->c <= 0xdfff)) unmapped++;
                }
            }
        }
        pdf_document *pdf = pdf_specifics(s->ctx, s->doc);
        pdf_page *pdf_page = pdf_page_from_fz_page(s->ctx, page);
        if (!pdf || !pdf_page) fz_throw(s->ctx, FZ_ERROR_FORMAT, "not a PDF page");
        pdf_obj *page_obj = pdf_lookup_page_obj(s->ctx, pdf, (int)number - 1);
        fz_rect media;
        fz_matrix transform;
        pdf_page_transform(s->ctx, pdf_page, &media, &transform);
        fz_matrix to_pdf = fz_invert_matrix(transform);
        int rotation = pdf_to_int(s->ctx, pdf_dict_get_inheritable(s->ctx, page_obj, PDF_NAME(Rotate))) % 360;
        if (rotation < 0) rotation += 360;
        writer = fz_new_output(s->ctx, 4096, out, write_output, NULL, NULL);
        fz_write_printf(s->ctx, writer,
            "{\"abi\":1,\"page\":%u,\"bounds\":[%g,%g,%g,%g],\"characters\":%zu,\"unmapped\":%zu,\"rotation\":%d,\"page_size\":[%g,%g],\"to_pdf\":[%g,%g,%g,%g,%g,%g],\"structured\":",
            number, bounds.x0, bounds.y0, bounds.x1, bounds.y1, chars, unmapped, rotation, media.x1 - media.x0, media.y1 - media.y0,
            to_pdf.a, to_pdf.b, to_pdf.c, to_pdf.d, to_pdf.e, to_pdf.f);
        fz_print_stext_page_as_json(s->ctx, writer, text, 1.0f);
        fz_write_string(s->ctx, writer, ",\"links\":[");
        /* fz_load_links resolves /URI /Base and turns GoTo actions into URI-like
         * strings. Read raw URI actions instead, while using native geometry. */
        pdf_obj *annots = pdf_dict_get(s->ctx, page_obj, PDF_NAME(Annots));
        int count = pdf_array_len(s->ctx, annots);
        if (count > 100000) fz_throw(s->ctx, FZ_ERROR_LIMIT, "page annotation limit exceeded");
        size_t link_count = 0;
        for (int i = 0; i < count; ++i) {
            pdf_obj *annot = pdf_array_get(s->ctx, annots, i);
            if (!pdf_name_eq(s->ctx, pdf_dict_get(s->ctx, annot, PDF_NAME(Subtype)), PDF_NAME(Link))) continue;
            pdf_obj *action = pdf_dict_get(s->ctx, annot, PDF_NAME(A));
            if (!pdf_name_eq(s->ctx, pdf_dict_get(s->ctx, action, PDF_NAME(S)), PDF_NAME(URI))) continue;
            pdf_obj *uri = pdf_dict_get(s->ctx, action, PDF_NAME(URI));
            if (!pdf_is_string(s->ctx, uri)) fz_throw(s->ctx, FZ_ERROR_FORMAT, "URI action has no string target");
            size_t uri_length = 0;
            const char *uri_bytes = pdf_to_string(s->ctx, uri, &uri_length);
            if (uri_length > 1000000) fz_throw(s->ctx, FZ_ERROR_LIMIT, "URI target exceeds limit");
            /* URI actions use ASCII strings. Keep literal bytes (including
             * escaped controls) instead of resolving, filtering, or rewriting. */
            pdf_obj *rect_obj = pdf_dict_get(s->ctx, annot, PDF_NAME(Rect));
            int has_rect = pdf_is_array(s->ctx, rect_obj) && pdf_array_len(s->ctx, rect_obj) == 4;
            for (int n = 0; has_rect && n < 4; ++n)
                has_rect = pdf_is_number(s->ctx, pdf_array_get(s->ctx, rect_obj, n));
            if (link_count++) fz_write_byte(s->ctx, writer, ',');
            fz_write_string(s->ctx, writer, "{\"uri\":");
            json_string(s->ctx, writer, uri_bytes, uri_length);
            if (has_rect) {
                fz_rect rect = fz_transform_rect(pdf_to_rect(s->ctx, rect_obj), transform);
                fz_write_printf(s->ctx, writer, ",\"bounds\":[%g,%g,%g,%g]}",
                    rect.x0, rect.y0, rect.x1, rect.y1);
            } else fz_write_string(s->ctx, writer, ",\"bounds\":null}");
        }
        fz_write_printf(s->ctx, writer, "],\"warnings\":%u}", s->warnings);
        fz_close_output(s->ctx, writer);
        status = 0;
    }
    fz_always(s->ctx) {
        fz_drop_output(s->ctx, writer);
        fz_drop_stext_page(s->ctx, text);
        fz_drop_page(s->ctx, page);
    }
    fz_catch(s->ctx) { error_text(error, error_capacity, fz_caught_message(s->ctx)); }
    if (!status) { *data = out->data; *len = out->len; } else free(out->data);
    free(out);
    return status;
}
