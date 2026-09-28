//! Sentence-aware sliding-window chunking of page text.

/// One indexed unit of text: a window of words taken from a single page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// Content hash of the source document (the ledger's `documents.hash`).
    pub doc_hash: String,
    /// Ledger run the text came from.
    pub run_id: i64,
    /// 1-based page number as stored in the ledger.
    pub page: u32,
    /// Position of the chunk within its document (0-based, across pages).
    pub idx: u32,
    /// The chunk text: the window's words joined by single spaces.
    pub text: String,
}

/// Cuts text into windows of about `window_words` words that overlap by
/// `overlap_words` words. A window is shortened to end at a sentence
/// boundary (`.`, `!` or `?` followed by a word starting with a capital
/// letter) when one falls in its second half.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chunker {
    /// Maximum words per window.
    pub window_words: usize,
    /// Words repeated at the start of the next window.
    pub overlap_words: usize,
}

impl Default for Chunker {
    fn default() -> Self {
        Self {
            window_words: 200,
            overlap_words: 40,
        }
    }
}

impl Chunker {
    /// Split `text` into overlapping word windows. Whitespace is collapsed;
    /// no other change is made to the words.
    pub fn windows(&self, text: &str) -> Vec<String> {
        let words: Vec<&str> = text.split_whitespace().collect();
        if words.is_empty() {
            return Vec::new();
        }
        let window = self.window_words.max(1);
        let overlap = self.overlap_words.min(window - 1);
        let starts = sentence_starts(&words);
        let mut out: Vec<String> = Vec::new();
        let mut start: usize = 0;
        loop {
            let hard_end = (start + window).min(words.len());
            let end = if hard_end < words.len() {
                // Only accept a boundary past the middle of the non-overlap part,
                // so every window advances by more than the overlap.
                let floor = start + overlap + (window - overlap) / 2;
                starts
                    .iter()
                    .rev()
                    .copied()
                    .find(|&b| b > floor && b <= hard_end)
                    .unwrap_or(hard_end)
            } else {
                hard_end
            };
            out.push(words[start..end].join(" "));
            if end >= words.len() {
                break;
            }
            start = end - overlap;
        }
        out
    }

    /// Chunk one page. Chunk indices start at `first_idx` and increase by one.
    pub fn chunk_page(
        &self,
        doc_hash: &str,
        run_id: i64,
        page: u32,
        first_idx: u32,
        text: &str,
    ) -> Vec<Chunk> {
        let mut idx = first_idx;
        let mut out: Vec<Chunk> = Vec::new();
        for window in self.windows(text) {
            out.push(Chunk {
                doc_hash: doc_hash.to_string(),
                run_id,
                page,
                idx,
                text: window,
            });
            idx = idx.saturating_add(1);
        }
        out
    }
}

/// Indices of words that start a new sentence: the previous word ends with
/// `.`, `!` or `?` (ignoring closing quotes and brackets) and this word
/// starts with an upper-case letter.
fn sentence_starts(words: &[&str]) -> Vec<usize> {
    let mut starts: Vec<usize> = Vec::new();
    for (i, pair) in words.windows(2).enumerate() {
        let prev = pair[0].trim_end_matches(['"', '\'', ')', ']', '\u{201d}', '\u{2019}']);
        let ends_sentence = prev.ends_with(['.', '!', '?']);
        let capital = pair[1].chars().next().is_some_and(char::is_uppercase);
        if ends_sentence && capital {
            starts.push(i + 1);
        }
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered_words(n: usize) -> String {
        (0..n)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn empty_and_short_text() {
        let c = Chunker::default();
        assert!(c.windows("   \n ").is_empty());
        assert_eq!(
            c.windows("one  two\nthree"),
            vec!["one two three".to_string()]
        );
    }

    #[test]
    fn windows_overlap_by_forty_words_without_sentences() {
        let c = Chunker::default();
        let text = numbered_words(500);
        let w = c.windows(&text);
        assert_eq!(w.len(), 3);
        let first: Vec<&str> = w[0].split(' ').collect();
        let second: Vec<&str> = w[1].split(' ').collect();
        let third: Vec<&str> = w[2].split(' ').collect();
        assert_eq!(first.len(), 200);
        assert_eq!(first[0], "w0");
        assert_eq!(second[0], "w160");
        assert_eq!(&first[160..], &second[..40]);
        assert_eq!(third[0], "w320");
        assert_eq!(*third.last().unwrap(), "w499");
    }

    #[test]
    fn windows_end_at_sentence_boundaries() {
        // Ten sentences of 30 words each: "Sentence ... end."
        let mut sentences: Vec<String> = Vec::new();
        for s in 0..10 {
            let mut words: Vec<String> = vec![format!("Start{s}")];
            for j in 1..29 {
                words.push(format!("x{s}_{j}"));
            }
            words.push(format!("end{s}."));
            sentences.push(words.join(" "));
        }
        let text = sentences.join(" ");
        let c = Chunker::default();
        let w = c.windows(&text);
        assert_eq!(w.len(), 2);
        let first: Vec<&str> = w[0].split(' ').collect();
        // Largest boundary in (120, 200] is word 180.
        assert_eq!(first.len(), 180);
        assert_eq!(*first.last().unwrap(), "end5.");
        let second: Vec<&str> = w[1].split(' ').collect();
        assert_eq!(&first[140..], &second[..40]);
        assert_eq!(*second.last().unwrap(), "end9.");
    }

    #[test]
    fn lowercase_after_period_is_not_a_boundary() {
        let words = ["e.g.", "this", "Fig.", "Two", "ends!", "Next"];
        assert_eq!(sentence_starts(&words), vec![3, 5]);
    }

    #[test]
    fn chunk_page_numbers_chunks() {
        let c = Chunker {
            window_words: 10,
            overlap_words: 2,
        };
        let chunks = c.chunk_page("abc", 7, 3, 5, &numbered_words(25));
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].idx, 5);
        assert_eq!(chunks[2].idx, 7);
        assert!(
            chunks
                .iter()
                .all(|ch| ch.page == 3 && ch.run_id == 7 && ch.doc_hash == "abc")
        );
    }
}
