// Optional Poppler 26.09.0 provider. See README.md for its GPL obligations.
#include "../provider.h"
#include <Annot.h>
#include <Error.h>
#include <ErrorCodes.h>
#include <GlobalParams.h>
#include <Link.h>
#include <PDFDoc.h>
#include <Page.h>
#include <TextOutputDev.h>
#include <poppler-config.h>
#include <algorithm>
#include <charconv>
#include <cmath>
#include <cstdlib>
#include <cstring>
#include <limits>
#include <memory>
#include <mutex>
#include <stdexcept>
#include <string_view>

static_assert(std::string_view(POPPLER_VERSION) == "26.09.0",
              "This provider requires the exact Poppler 26.09.0 core ABI");

namespace {
constexpr size_t max_output = 16 * 1024 * 1024;
constexpr size_t max_characters = 1000000;
std::mutex engine_mutex;
thread_local uint32_t *active_warnings = nullptr;

void warn() noexcept {
    if (active_warnings && *active_warnings < UINT32_MAX) ++*active_warnings;
}
void on_error(ErrorCategory, Goffset, const char *) { warn(); }
struct WarningScope {
    uint32_t *previous;
    explicit WarningScope(uint32_t &count) : previous(active_warnings) {
        active_warnings = &count;
        setErrorCallback(on_error);
    }
    ~WarningScope() { active_warnings = previous; }
};
struct Session {
    std::unique_ptr<PDFDoc> doc;
    uint32_t open_warnings = 0;
};
void error_text(char *target, size_t capacity, const char *message) noexcept {
    if (!target || !capacity) return;
    const size_t length = std::min(capacity - 1, std::strlen(message));
    std::memcpy(target, message, length);
    target[length] = 0;
}

// One allocation, bounded before construction. No intermediate page-sized JSON.
struct Json {
    std::unique_ptr<unsigned char, decltype(&std::free)> bytes;
    size_t length = 0;
    size_t capacity;
    explicit Json(size_t limit) : bytes(nullptr, &std::free), capacity(std::min(limit, max_output)) {
        if (!capacity) throw std::runtime_error("zero provider output limit");
        bytes.reset(static_cast<unsigned char *>(std::malloc(capacity)));
        if (!bytes) throw std::bad_alloc();
    }
    void put(std::string_view value) {
        if (value.size() > capacity - length) throw std::runtime_error("provider output limit exceeded");
        std::memcpy(bytes.get() + length, value.data(), value.size());
        length += value.size();
    }
    void number(double value) {
        if (!std::isfinite(value)) throw std::runtime_error("nonfinite native geometry");
        char buffer[64];
        auto result = std::to_chars(buffer, buffer + sizeof(buffer), value);
        if (result.ec != std::errc()) throw std::runtime_error("invalid native geometry");
        put({buffer, static_cast<size_t>(result.ptr - buffer)});
    }
    void integer(uint64_t value) {
        char buffer[32];
        auto result = std::to_chars(buffer, buffer + sizeof(buffer), value);
        put({buffer, static_cast<size_t>(result.ptr - buffer)});
    }
    void scalar(uint32_t value) {
        if (value == '"') { put("\\\""); return; }
        if (value == '\\') { put("\\\\"); return; }
        if (value < 32) {
            const char hex[] = "0123456789abcdef";
            const char escaped[] = {'\\', 'u', '0', '0', hex[value >> 4], hex[value & 15]};
            put({escaped, sizeof(escaped)});
            return;
        }
        char encoded[4];
        size_t n;
        if (value < 0x80) { encoded[0] = static_cast<char>(value); n = 1; }
        else if (value < 0x800) {
            encoded[0] = static_cast<char>(0xc0 | value >> 6);
            encoded[1] = static_cast<char>(0x80 | (value & 63)); n = 2;
        } else if (value < 0x10000) {
            encoded[0] = static_cast<char>(0xe0 | value >> 12);
            encoded[1] = static_cast<char>(0x80 | ((value >> 6) & 63));
            encoded[2] = static_cast<char>(0x80 | (value & 63)); n = 3;
        } else {
            encoded[0] = static_cast<char>(0xf0 | value >> 18);
            encoded[1] = static_cast<char>(0x80 | ((value >> 12) & 63));
            encoded[2] = static_cast<char>(0x80 | ((value >> 6) & 63));
            encoded[3] = static_cast<char>(0x80 | (value & 63)); n = 4;
        }
        put({encoded, n});
    }
    // URI/font bytes are accepted only as valid UTF-8. Invalid sequences retain
    // replacement evidence and mark extraction incomplete, never invalid JSON.
    void string(std::string_view input) {
        put("\"");
        for (size_t i = 0; i < input.size();) {
            const auto first = static_cast<unsigned char>(input[i]);
            uint32_t value = first;
            size_t n = 1;
            if (first >= 0xc2 && first <= 0xdf) { n = 2; value &= 31; }
            else if (first >= 0xe0 && first <= 0xef) { n = 3; value &= 15; }
            else if (first >= 0xf0 && first <= 0xf4) { n = 4; value &= 7; }
            bool valid = first < 0x80 || n > 1;
            if (n > input.size() - i) valid = false;
            if (valid) for (size_t j = 1; j < n; ++j) {
                const auto byte = static_cast<unsigned char>(input[i + j]);
                if ((byte & 0xc0) != 0x80) { valid = false; break; }
                value = (value << 6) | (byte & 63);
            }
            if ((n == 2 && value < 0x80) || (n == 3 && value < 0x800) ||
                (n == 4 && value < 0x10000) || value > 0x10ffff ||
                (value >= 0xd800 && value <= 0xdfff)) valid = false;
            if (!valid) { value = 0xfffd; n = 1; warn(); }
            scalar(value); i += n;
        }
        put("\"");
    }
    void bounds(const PDFRectangle &r) {
        put("["); number(r.x1); put(","); number(r.y1); put(","); number(r.x2); put(","); number(r.y2); put("]");
    }
    void rect(const PDFRectangle &r) {
        if (r.x2 < r.x1 || r.y2 < r.y1) throw std::runtime_error("inverted native geometry");
        put("{\"x\":"); number(r.x1); put(",\"y\":"); number(r.y1);
        put(",\"w\":"); number(r.x2 - r.x1); put(",\"h\":"); number(r.y2 - r.y1); put("}");
    }
};

bool invalid_scalar(uint32_t value) {
    return value == 0 || value == 0xfffd || value > 0x10ffff ||
           (value >= 0xd800 && value <= 0xdfff);
}
class TextDevice final : public TextOutputDev {
public:
    TextDevice() : TextOutputDev(static_cast<const char *>(nullptr), false, 0, false, false) {}
    void drawChar(GfxState *state, double x, double y, double dx, double dy,
                  double ox, double oy, CharCode code, int nbytes,
                  const Unicode *unicode, int length) override {
        // The upstream default silently omits glyphs with no Unicode mapping.
        // Retain an explicit replacement at that glyph's native geometry.
        const Unicode replacement = 0xfffd;
        if (length <= 0 || !unicode) {
            unicode = &replacement; length = 1; warn();
        }
        TextOutputDev::drawChar(state, x, y, dx, dy, ox, oy, code, nbytes, unicode, length);
    }
};

PDFRectangle transformed(const PDFRectangle &r, const double *m) {
    PDFRectangle out{std::numeric_limits<double>::infinity(), std::numeric_limits<double>::infinity(),
                     -std::numeric_limits<double>::infinity(), -std::numeric_limits<double>::infinity()};
    for (double x : {r.x1, r.x2}) for (double y : {r.y1, r.y2}) {
        const double tx = m[0] * x + m[2] * y + m[4];
        const double ty = m[1] * x + m[3] * y + m[5];
        if (!std::isfinite(tx) || !std::isfinite(ty)) throw std::runtime_error("invalid native transform");
        out.x1 = std::min(out.x1, tx); out.x2 = std::max(out.x2, tx);
        out.y1 = std::min(out.y1, ty); out.y2 = std::max(out.y2, ty);
    }
    return out;
}
} // namespace

extern "C" {
uint32_t tpe_pdf_provider_abi_version() { return 1; }
const char *tpe_pdf_provider_engine() { return "poppler"; }
const char *tpe_pdf_provider_version() { return POPPLER_VERSION; }
const char *tpe_pdf_provider_runtime_symbol() { return "globalParams"; }
uintptr_t tpe_pdf_provider_runtime_anchor() { return reinterpret_cast<uintptr_t>(&globalParams); }

int tpe_pdf_provider_open(const unsigned char *bytes, size_t length, const char *password,
                         void **handle, uint32_t *pages, char *error, size_t error_capacity) {
    if (handle) *handle = nullptr;
    if (pages) *pages = 0;
    error_text(error, error_capacity, "");
    try {
        if (!handle || !pages || !bytes || !length || length > static_cast<size_t>(std::numeric_limits<Goffset>::max()))
            throw std::runtime_error("invalid native document input");
        std::lock_guard lock(engine_mutex);
        auto session = std::make_unique<Session>();
        WarningScope scope(session->open_warnings);
        if (!globalParams) globalParams = std::make_unique<GlobalParams>();
        std::optional<GooString> pw;
        if (password && *password) pw.emplace(password);
        auto stream = std::make_unique<MemStream>(reinterpret_cast<const char *>(bytes), 0,
                                                 static_cast<Goffset>(length), Object::null());
        session->doc = std::make_unique<PDFDoc>(std::move(stream), pw, pw, [] { warn(); });
        if (!session->doc->isOk()) {
            if (session->doc->getErrorCode() == errEncrypted) {
                error_text(error, error_capacity, "PDF password required or incorrect");
                return pw ? 3 : 2;
            }
            throw std::runtime_error("Poppler could not open PDF");
        }
        const int count = session->doc->getNumPages();
        if (count <= 0) throw std::runtime_error("PDF has no pages");
        *pages = static_cast<uint32_t>(count);
        *handle = session.release();
        return 0;
    } catch (const std::exception &e) { error_text(error, error_capacity, e.what()); }
      catch (...) { error_text(error, error_capacity, "unknown Poppler open failure"); }
    return 1;
}

int tpe_pdf_provider_page(void *handle, uint32_t number, size_t output_limit,
                         unsigned char **data, size_t *length, char *error, size_t error_capacity) {
    if (data) *data = nullptr;
    if (length) *length = 0;
    error_text(error, error_capacity, "");
    try {
        if (!handle || !data || !length) throw std::runtime_error("invalid native page request");
        std::lock_guard lock(engine_mutex);
        auto &session = *static_cast<Session *>(handle);
        if (!number || number > static_cast<uint32_t>(session.doc->getNumPages()))
            throw std::runtime_error("native page outside document");
        uint32_t warnings = session.open_warnings;
        WarningScope scope(warnings);
        Page *page = session.doc->getPage(static_cast<int>(number));
        if (!page || !page->isOk()) throw std::runtime_error("invalid native page");
        TextDevice text;
        session.doc->displayPage(&text, static_cast<int>(number), 72, 72, 0, false, true, false);
        double matrix[6];
        page->getDefaultCTM(matrix, 72, 72, 0, false, true);
        const PDFRectangle bounds = transformed(page->getCropBox(), matrix);
        Json json(output_limit);
        json.put("{\"abi\":1,\"page\":"); json.integer(number);
        json.put(",\"bounds\":"); json.bounds(bounds);
        const double determinant = matrix[0] * matrix[3] - matrix[1] * matrix[2];
        if (!std::isfinite(determinant) || determinant == 0) throw std::runtime_error("singular native page transform");
        const double inverse[] = {
            matrix[3] / determinant, -matrix[1] / determinant,
            -matrix[2] / determinant, matrix[0] / determinant,
            (matrix[2] * matrix[5] - matrix[3] * matrix[4]) / determinant,
            (matrix[1] * matrix[4] - matrix[0] * matrix[5]) / determinant
        };
        json.put(",\"to_pdf\":[");
        for (size_t i = 0; i < 6; ++i) { if (i) json.put(","); json.number(inverse[i]); }
        json.put("],\"page_size\":["); json.number(page->getCropWidth());
        json.put(","); json.number(page->getCropHeight());
        json.put("],\"rotation\":"); json.integer(static_cast<uint32_t>(page->getRotate()));
        json.put(",\"structured\":{\"blocks\":[");
        size_t characters = 0, unmapped = 0, blocks = 0;
        for (const TextFlow *flow = text.getFlows(); flow; flow = flow->getNext())
            for (const TextBlock *block = flow->getBlocks(); block; block = block->getNext()) {
                if (blocks++) json.put(",");
                json.put("{\"type\":\"text\",\"bbox\":"); json.rect(block->getBBox()); json.put(",\"lines\":[");
                size_t runs = 0;
                for (const TextLine *line = block->getLines(); line; line = line->getNext())
                    for (const TextWord *word = line->getWords(); word; word = word->getNext()) {
                        if (word->getLength() <= 0) continue;
                        // Each word is one positioned font run. Rust owns all
                        // subsequent reading-order and bibliography processing.
                        if (runs++) json.put(",");
                        json.put("{\"bbox\":"); json.rect(word->getBBox()); json.put(",\"font\":{\"name\":");
                        const auto *font = word->getFontInfo(0);
                        const GooString *name = font ? font->getFontName() : nullptr;
                        json.string(name ? name->toStr() : "unknown");
                        json.put(",\"size\":"); json.number(word->getFontSize());
                        json.put(",\"weight\":"); json.string(font && font->isBold() ? "bold" : "normal");
                        json.put(",\"style\":"); json.string(font && font->isItalic() ? "italic" : "normal");
                        json.put("},\"text\":\"");
                        for (int i = 0; i < word->getLength(); ++i) {
                            uint32_t value = *word->getChar(i);
                            if (invalid_scalar(value)) { value = 0xfffd; ++unmapped; }
                            if (++characters > max_characters) throw std::runtime_error("native page character limit exceeded");
                            json.scalar(value);
                        }
                        if (word->getSpaceAfter()) { json.put(" "); ++characters; }
                        if (characters > max_characters) throw std::runtime_error("native page character limit exceeded");
                        json.put("\"}");
                    }
                json.put("]}");
            }
        json.put("]},\"links\":[");
        Object annotations = page->getAnnotsObject();
        size_t link_count = 0;
        if (annotations.isArray()) for (int index = 0; index < annotations.arrayGetLength(); ++index) {
            Object link = annotations.arrayGet(index);
            if (!link.isDict()) { warn(); continue; }
            if (!link.dictLookup("Subtype").isName("Link")) continue;
            Object action = link.dictLookup("A");
            if (!action.isDict()) continue;
            Object kind = action.dictLookup("S");
            if (!kind.isName("URI")) continue;
            Object uri = action.dictLookup("URI");
            if (!uri.isString()) { warn(); continue; }
            if (++link_count > 100000) throw std::runtime_error("native page link limit exceeded");
            if (link_count > 1) json.put(",");
            // LinkURI::getURI resolves /URI /Base and adds http:// to www.
            // Retain the raw action string instead; Rust owns normalization.
            json.put("{\"uri\":"); json.string(uri.getString());
            json.put(",\"bounds\":");
            Object raw_rect = link.dictLookup("Rect");
            bool valid_rect = raw_rect.isArray() && raw_rect.arrayGetLength() == 4;
            double coords[4]{};
            if (valid_rect) for (int i = 0; i < 4; ++i) {
                Object coordinate = raw_rect.arrayGet(i);
                if (!coordinate.isNum() || !std::isfinite(coordinate.getNum())) valid_rect = false;
                else coords[i] = coordinate.getNum();
            }
            if (valid_rect) json.bounds(transformed(PDFRectangle{coords[0], coords[1], coords[2], coords[3]}, matrix));
            else { json.put("null"); warn(); }
            json.put("}");
        }
        json.put("],\"characters\":"); json.integer(characters);
        json.put(",\"unmapped\":"); json.integer(unmapped);
        json.put(",\"warnings\":"); json.integer(warnings); json.put("}");
        *length = json.length; *data = json.bytes.release();
        return 0;
    } catch (const std::exception &e) { error_text(error, error_capacity, e.what()); }
      catch (...) { error_text(error, error_capacity, "unknown Poppler page failure"); }
    return 1;
}
void tpe_pdf_provider_free(unsigned char *data) { std::free(data); }
void tpe_pdf_provider_close(void *handle) {
    try { std::lock_guard lock(engine_mutex); delete static_cast<Session *>(handle); }
    catch (...) { /* No exception may cross the C ABI. */ }
}
} // extern C
