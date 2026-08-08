//! Choosing which pages of a document to put in front of the model.
//!
//! A local model's context is small and its attention is finite, so sending a
//! whole document is both slow and worse: the answer drowns. This picks the
//! pages a question is actually about, using a stripped-down BM25 -- term
//! frequency saturated so a page cannot win by repeating one word, times
//! inverse document frequency so words common to every page count for little.
//! No length normalization: pages are already roughly one size, and the
//! character budget below is the real limit on how much any page contributes.
//!
//! Everything here is a pure function of the page text and the question, which
//! is what makes it testable without a model, a document or a thread.

/// How much page text one request may carry. Roughly 3k tokens, which leaves
/// room for the question and the answer in an 8k-token context.
pub const CHAR_BUDGET: usize = 12_000;
/// Ceiling on how many pages get quoted, budget notwithstanding.
pub const MAX_PAGES: usize = 8;
/// Term-frequency saturation constant (BM25's `k1`).
const K1: f32 = 1.2;
/// Share of the best page's score a page must reach to be quoted at all.
/// Without it the budget fills with pages that matched only "the" and "is",
/// which costs the model attention and buys nothing.
const RELEVANCE_FLOOR: f32 = 0.25;

/// Lowercase alphanumeric runs. Punctuation, hyphenation and case all vanish,
/// which is what makes "Section 4.1" match "section 4 1".
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Which pages to quote for `question`, as 0-based indices in page order.
///
/// Pages far behind the best match are left out; when nothing matches at all
/// (an empty question, or one sharing no words with the document) the opening
/// pages are used instead, since a title page answers "what is this?" better
/// than nothing does.
pub fn select_pages(pages: &[String], question: &str) -> Vec<usize> {
    if pages.is_empty() {
        return Vec::new();
    }
    let scores = score_pages(pages, question);
    let best = scores.iter().copied().fold(0.0f32, f32::max);
    let floor = best * RELEVANCE_FLOOR;
    let mut ranked: Vec<(usize, f32)> = scores
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, score)| *score > 0.0 && *score >= floor)
        .collect();
    // Highest score first; ties go to the earlier page.
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));

    let order: Vec<usize> = if ranked.is_empty() {
        (0..pages.len()).collect()
    } else {
        ranked.into_iter().map(|(page, _)| page).collect()
    };

    let mut chosen = Vec::new();
    let mut used = 0usize;
    for page in order {
        if chosen.len() >= MAX_PAGES || used >= CHAR_BUDGET {
            break;
        }
        chosen.push(page);
        used += pages[page].chars().count().min(CHAR_BUDGET);
    }
    // Reading order, not ranking order: the model sees the document's own
    // sequence, and consecutive pages read as continuous prose.
    chosen.sort_unstable();
    chosen
}

/// BM25-lite relevance of every page to `question`.
fn score_pages(pages: &[String], question: &str) -> Vec<f32> {
    let terms: Vec<String> = {
        let mut terms = tokenize(question);
        terms.sort();
        terms.dedup();
        terms
    };
    let mut scores = vec![0.0f32; pages.len()];
    if terms.is_empty() {
        return scores;
    }

    let tokenized: Vec<Vec<String>> = pages.iter().map(|p| tokenize(p)).collect();
    let n = pages.len() as f32;
    for term in &terms {
        let counts: Vec<usize> = tokenized
            .iter()
            .map(|tokens| tokens.iter().filter(|t| *t == term).count())
            .collect();
        let df = counts.iter().filter(|c| **c > 0).count() as f32;
        if df == 0.0 {
            continue;
        }
        // Probabilistic idf: a term on every page contributes almost nothing,
        // a term on one page a great deal. Never negative (the +1 sees to it),
        // so a common word can't push a page's score down.
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        for (score, count) in scores.iter_mut().zip(counts) {
            if count > 0 {
                let tf = count as f32;
                *score += idf * (tf / (tf + K1));
            }
        }
    }
    scores
}

/// The quoted pages, labelled with the 1-based numbers the model is asked to
/// cite. A single page longer than the whole budget is cut short rather than
/// allowed to crowd out everything else.
pub fn context_block(pages: &[String], selected: &[usize]) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for &page in selected {
        let Some(text) = pages.get(page) else {
            continue;
        };
        let text = text.trim();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("[Page {}]\n", page + 1));
        let room = CHAR_BUDGET.saturating_sub(used);
        if text.chars().count() > room {
            out.extend(text.chars().take(room));
            out.push_str("\n[…page truncated…]");
            used = CHAR_BUDGET;
        } else {
            out.push_str(text);
            used += text.chars().count();
        }
    }
    out
}

/// The instructions the model answers under. Kept here beside the retrieval so
/// the promise it makes ("only these pages") is the same one `select_pages`
/// keeps.
///
/// Plus what to do with tools when the user has allowed some.
///
/// The document instruction is deliberately not softened: the tools are for
/// looking things up *outside* the document, and a fact from the document still
/// has to come from a quoted page.
pub fn system_prompt_with_tools(title: &str, tools: bool) -> String {
    let mut prompt = format!(
        "You are answering questions about a PDF document titled \"{title}\". \
         The pages quoted in the user's message are the only source you have; \
         do not use anything else you may know about this document. Cite the \
         page each fact came from as [p.N], using the page numbers shown in \
         the quoted text. If the quoted pages do not answer the question, say \
         so plainly and say what they do cover instead of guessing. Answer in \
         a few sentences unless asked for more."
    );
    if tools {
        prompt.push_str(
            " Tools are available for things outside the document; use one only \
             when the quoted pages cannot answer the question, and say what you \
             found with it.",
        );
    }
    prompt
}

/// The user message: the quoted pages, then the question.
pub fn user_prompt(title: &str, context: &str, question: &str) -> String {
    if context.is_empty() {
        return format!(
            "No text could be read from \"{title}\" (it may be a scan awaiting \
             OCR).\n\nQuestion: {question}"
        );
    }
    format!("Pages from \"{title}\":\n\n{context}\n\nQuestion: {question}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|t| (*t).to_owned()).collect()
    }

    #[test]
    fn tokenizing_keeps_alphanumeric_runs_and_folds_case() {
        assert_eq!(
            tokenize("Section 4.1 -- Fire-rated doors!"),
            ["section", "4", "1", "fire", "rated", "doors"]
        );
        assert!(tokenize("   ,.;  ").is_empty());
        // Non-ASCII letters are letters.
        assert_eq!(tokenize("Café Ångström"), ["café", "ångström"]);
    }

    #[test]
    fn the_page_that_mentions_the_question_wins() {
        let pages = pages(&[
            "Introduction. This document is about buildings.",
            "The fire alarm panel is located in the north stairwell.",
            "Appendix: a table of paint colours.",
        ]);
        assert_eq!(select_pages(&pages, "where is the fire alarm panel?"), [1]);
    }

    #[test]
    fn a_word_on_every_page_does_not_decide_the_ranking() {
        let pages = pages(&[
            "invoice terms and conditions",
            "invoice total is 4200 dollars",
            "invoice contact details",
        ]);
        // "invoice" is on every page, so "total" is what picks the page out.
        assert_eq!(select_pages(&pages, "invoice total"), [1]);
    }

    #[test]
    fn repeating_one_word_cannot_outrank_covering_two() {
        let pages = pages(&[
            "alarm alarm alarm alarm alarm alarm alarm alarm",
            "the alarm is wired to the panel",
            "nothing relevant here at all",
        ]);
        let scores = score_pages(&pages, "alarm panel");
        assert!(
            scores[1] > scores[0],
            "covering both terms should beat repeating one: {scores:?}"
        );
        assert_eq!(scores[2], 0.0);
    }

    #[test]
    fn pages_far_behind_the_best_match_are_left_out() {
        // Page 0 shares only "the" with the question; there is a page that
        // actually answers it, so quoting page 0 would just cost attention.
        let pages = pages(&[
            "the introduction, which is about buildings in general",
            "the fire alarm panel is in the north stairwell",
        ]);
        assert_eq!(select_pages(&pages, "where is the fire alarm panel?"), [1]);
    }

    #[test]
    fn selection_is_returned_in_reading_order() {
        let pages = pages(&[
            "the panel is here",
            "nothing",
            "the alarm is here",
            "nothing",
        ]);
        let chosen = select_pages(&pages, "alarm panel");
        assert_eq!(chosen, [0, 2]);
    }

    #[test]
    fn nothing_relevant_falls_back_to_the_opening_pages() {
        let pages = pages(&["first", "second", "third"]);
        assert_eq!(select_pages(&pages, "xyzzy plugh"), [0, 1, 2]);
        // An empty question is the same situation.
        assert_eq!(select_pages(&pages, ""), [0, 1, 2]);
        // And a document with no pages selects nothing rather than panicking.
        assert!(select_pages(&[], "anything").is_empty());
    }

    #[test]
    fn the_budget_caps_both_the_page_count_and_the_characters() {
        // Twenty short, equally relevant pages: the page ceiling binds.
        let many = pages(&["alarm"; 20]);
        assert_eq!(select_pages(&many, "alarm").len(), MAX_PAGES);

        // Four pages of half the budget each: the character budget binds first.
        let big: Vec<String> = (0..4).map(|_| "alarm ".repeat(CHAR_BUDGET / 12)).collect();
        let chosen = select_pages(&big, "alarm");
        assert!(chosen.len() <= 2, "got {} pages", chosen.len());
        assert!(!chosen.is_empty());
    }

    #[test]
    fn the_context_block_labels_pages_with_their_human_numbers() {
        let pages = pages(&["alpha", "beta", "gamma"]);
        let block = context_block(&pages, &[0, 2]);
        assert_eq!(block, "[Page 1]\nalpha\n\n[Page 3]\ngamma");
        assert!(context_block(&pages, &[]).is_empty());
        // An out-of-range page is skipped, not a panic.
        assert_eq!(context_block(&pages, &[9]), "");
    }

    #[test]
    fn an_oversized_page_is_truncated_rather_than_sent_whole() {
        let huge = pages(&["x".repeat(CHAR_BUDGET * 2).as_str()]);
        let block = context_block(&huge, &[0]);
        assert!(block.chars().count() < CHAR_BUDGET + 100);
        assert!(block.contains("page truncated"));
    }

    #[test]
    fn the_prompts_say_where_the_answer_must_come_from() {
        let system = system_prompt_with_tools("Plans.pdf", false);
        assert!(system.contains("Plans.pdf"));
        assert!(system.contains("[p.N]"));
        assert!(
            !system.contains("Tools"),
            "no tools were offered, so none are mentioned"
        );

        // With tools allowed the document instruction still stands: they are
        // for looking things up outside it, not for guessing what is in it.
        let with_tools = system_prompt_with_tools("Plans.pdf", true);
        assert!(with_tools.starts_with(&system), "{with_tools}");
        assert!(with_tools.contains("outside the document"), "{with_tools}");

        let user = user_prompt("Plans.pdf", "[Page 1]\nhello", "what is this?");
        assert!(user.contains("[Page 1]"));
        assert!(user.ends_with("what is this?"));

        // A scan with no text layer says so rather than pretending.
        let empty = user_prompt("Scan.pdf", "", "what is this?");
        assert!(empty.contains("OCR"));
    }
}
