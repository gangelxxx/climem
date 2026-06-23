//! Chunking that follows the document's structure, with a bit of overlap
//! (desc.md §6). Markdown splits on headings; plain text and HTML split on
//! paragraphs with a word overlap; anything still too big gets split again by
//! size. Every chunk keeps an `origin` breadcrumb that says where it came from
//! (e.g. `› Spec › Auth`, or `#3`).

pub struct Chunk {
    pub text: String,
    /// Structural position within the source, appended to the file name.
    pub origin: String,
}

/// Split Markdown on headings; sections that are still too big get split by size
/// with overlap.
///
/// `origin` holds the full heading breadcrumb (`› H1 › H2 › H3`), which we build
/// from a level-aware stack. We also prefix the ancestor path into the chunk
/// body itself, so the keyword and vector channels see some surrounding context.
/// A few prefix words buy us recall more cheaply than a bigger chunk would
/// (token-efficiency-plan R3), and we charge those words against the budget so
/// chunks don't quietly grow.
pub fn markdown(src: &str, max_words: usize, overlap: usize) -> Vec<Chunk> {
    // (breadcrumb titles incl. leaf, body incl. own heading line)
    let mut sections: Vec<(Vec<String>, String)> = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new(); // (heading level, title)
    let mut cur_path: Vec<String> = Vec::new();
    let mut cur_buf = String::new();

    for line in src.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            // flush previous section
            if !cur_buf.trim().is_empty() {
                // `cur_buf` is replaced below, so move it out instead of cloning.
                sections.push((cur_path.clone(), std::mem::take(&mut cur_buf)));
            }
            let level = trimmed.chars().take_while(|c| *c == '#').count();
            let title = trimmed.trim_start_matches('#').trim().to_string();
            // pop siblings/deeper headings so the stack is this heading's ancestry
            while matches!(stack.last(), Some((l, _)) if *l >= level) {
                stack.pop();
            }
            stack.push((level, title));
            cur_path = stack.iter().map(|(_, t)| t.clone()).collect();
            cur_buf.clear();
            cur_buf.push_str(line);
            cur_buf.push('\n');
        } else {
            cur_buf.push_str(line);
            cur_buf.push('\n');
        }
    }
    if !cur_buf.trim().is_empty() {
        // Final flush: both are dead after this, so move rather than clone.
        sections.push((cur_path, cur_buf));
    }
    if sections.is_empty() {
        return text(src, max_words, overlap);
    }

    let mut out = Vec::new();
    for (path, body) in sections {
        let label = if path.is_empty() {
            String::new()
        } else {
            format!("› {}", path.join(" › "))
        };
        // Ancestor breadcrumb. The leaf is left out: its heading line is already in the body.
        let ancestors = if path.len() > 1 {
            path[..path.len() - 1].join(" › ")
        } else {
            String::new()
        };
        let with_prefix = |content: &str| -> String {
            if ancestors.is_empty() {
                content.to_string()
            } else {
                format!("{ancestors}\n{content}")
            }
        };
        let body = body.trim();
        let words: Vec<&str> = body.split_whitespace().collect();
        // Charge the prefix against the budget so prefixed chunks stay bounded.
        let budget = max_words
            .saturating_sub(ancestors.split_whitespace().count())
            .max(1);
        if words.len() <= budget {
            out.push(Chunk {
                text: with_prefix(body),
                origin: label,
            });
        } else {
            for (i, w) in window(&words, budget, overlap).into_iter().enumerate() {
                let suffix = if label.is_empty() {
                    format!("(part {})", i + 1)
                } else {
                    format!("{label} (part {})", i + 1)
                };
                out.push(Chunk {
                    text: with_prefix(&w),
                    origin: suffix,
                });
            }
        }
    }
    out
}

/// Split plain text into paragraph-packed chunks with word overlap.
pub fn text(src: &str, max_words: usize, overlap: usize) -> Vec<Chunk> {
    // Paragraphs separated by blank lines.
    let paras: Vec<String> = src
        .split("\n\n")
        .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|p| !p.is_empty())
        .collect();

    if paras.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut buf: Vec<&str> = Vec::new();
    let mut idx = 0usize;

    let flush = |buf: &mut Vec<&str>, idx: &mut usize, out: &mut Vec<Chunk>| {
        if buf.is_empty() {
            return;
        }
        out.push(Chunk {
            text: buf.join(" "),
            origin: format!("#{}", *idx + 1),
        });
        *idx += 1;
    };

    for para in &paras {
        for word in para.split_whitespace() {
            buf.push(word);
            if buf.len() >= max_words {
                // carry the last `overlap` words into the next chunk so we don't
                // cut a thought in half at the boundary
                let tail: Vec<&str> = if overlap > 0 && overlap < max_words {
                    buf.iter().rev().take(overlap).rev().cloned().collect()
                } else {
                    Vec::new()
                };
                flush(&mut buf, &mut idx, &mut out);
                buf.clear();
                buf.extend(tail);
            }
        }
    }
    flush(&mut buf, &mut idx, &mut out);
    out
}

/// Sliding windows of `size` words advancing by `size - overlap`.
fn window(words: &[&str], size: usize, overlap: usize) -> Vec<String> {
    let step = if size > overlap {
        size - overlap
    } else {
        size.max(1)
    };
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < words.len() {
        let end = (start + size).min(words.len());
        out.push(words[start..end].join(" "));
        if end == words.len() {
            break;
        }
        start += step;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (text, origin) pairs — Chunk has no PartialEq, so compare fields.
    fn parts(cs: &[Chunk]) -> Vec<(&str, &str)> {
        cs.iter()
            .map(|c| (c.text.as_str(), c.origin.as_str()))
            .collect()
    }

    #[test]
    fn text_empty_input_returns_empty() {
        assert!(text("", 10, 2).is_empty());
        assert!(text("   \n  ", 10, 2).is_empty());
    }

    #[test]
    fn text_short_single_chunk_origin_hash1() {
        assert_eq!(
            parts(&text("hello world", 10, 2)),
            vec![("hello world", "#1")]
        );
    }

    #[test]
    fn text_overlap_carries_last_n_words() {
        let cs = text("a b c d e", 2, 1);
        assert_eq!(cs[0].text, "a b");
        assert_eq!(cs[0].origin, "#1");
        // The last word of chunk 1 is carried into chunk 2.
        assert_eq!(cs[1].text, "b c");
        assert_eq!(cs[1].origin, "#2");
    }

    #[test]
    fn text_overlap_zero_no_carry() {
        let cs = text("a b c d", 2, 0);
        assert_eq!(parts(&cs), vec![("a b", "#1"), ("c d", "#2")]);
    }

    #[test]
    fn text_overlap_ge_max_words_no_carry() {
        // overlap (5) >= max_words (2): the carry branch is skipped, no overlap.
        let cs = text("a b c d", 2, 5);
        assert_eq!(parts(&cs), vec![("a b", "#1"), ("c d", "#2")]);
    }

    #[test]
    fn text_collapses_internal_whitespace() {
        let cs = text("a   b\n c\t d", 10, 2);
        assert_eq!(cs[0].text, "a b c d");
    }

    #[test]
    fn text_multiple_paragraphs_packed_and_indexed() {
        // Packing crosses paragraph boundaries; origin numbers emitted chunks.
        let cs = text("a b\n\nc d\n\ne f", 3, 0);
        assert_eq!(parts(&cs), vec![("a b c", "#1"), ("d e f", "#2")]);
    }

    #[test]
    fn text_max_words_one_splits_per_word() {
        let cs = text("a b c", 1, 0);
        assert_eq!(parts(&cs), vec![("a", "#1"), ("b", "#2"), ("c", "#3")]);
    }

    #[test]
    fn markdown_splits_by_heading_with_breadcrumb_origin() {
        // `## Auth` nests under `# Intro` -> full breadcrumb path, and the
        // ancestor ("Intro") is prefixed into the nested chunk's body.
        let cs = markdown("# Intro\nbody1\n## Auth\nbody2", 100, 10);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].origin, "› Intro");
        assert!(cs[0].text.contains("body1"));
        assert_eq!(cs[1].origin, "› Intro › Auth");
        assert!(cs[1].text.contains("body2"));
        assert!(
            cs[1].text.starts_with("Intro\n"),
            "ancestor prefixed into body"
        );
    }

    #[test]
    fn markdown_no_heading_is_single_untitled_section() {
        // Non-blank input without a heading does NOT delegate to text(): it
        // becomes one section with an empty title -> origin "". (Delegation to
        // text() only happens when the input is entirely blank.)
        let cs = markdown("just some text here", 100, 10);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].origin, "");
    }

    #[test]
    fn markdown_preamble_before_first_heading_empty_label() {
        let cs = markdown("preamble line\n# Head\nbody", 100, 10);
        assert_eq!(cs[0].origin, "");
        assert!(cs[0].text.contains("preamble line"));
        assert_eq!(cs[1].origin, "› Head");
    }

    #[test]
    fn markdown_oversized_section_splits_with_part_suffix() {
        // "# Big" + 8 words = 10 whitespace tokens > max_words 5.
        let cs = markdown("# Big\nw1 w2 w3 w4 w5 w6 w7 w8", 5, 2);
        assert!(cs.len() > 1);
        assert_eq!(cs[0].origin, "› Big (part 1)");
        assert_eq!(cs[1].origin, "› Big (part 2)");
    }

    #[test]
    fn markdown_oversized_preamble_part_without_arrow() {
        // Preamble (empty title) that is oversized -> "(part N)" without "›".
        let cs = markdown("w1 w2 w3 w4 w5\n# H\nbody", 3, 1);
        assert_eq!(cs[0].origin, "(part 1)");
        assert!(!cs[0].origin.contains('›'));
    }

    #[test]
    fn markdown_heading_only_no_body() {
        // `## Another` nests under `# Heading`: breadcrumb origin + ancestor
        // prefixed into the body (which is just the heading line here).
        let cs = markdown("# Heading\n## Another", 100, 10);
        assert_eq!(
            parts(&cs),
            vec![
                ("# Heading", "› Heading"),
                ("Heading\n## Another", "› Heading › Another")
            ]
        );
    }

    #[test]
    fn markdown_breadcrumb_tracks_three_levels_and_pops_siblings() {
        // H1 A > H2 B > H3 C, then a sibling H2 D pops C and reparents under A.
        let cs = markdown("# A\n## B\n### C\ncc\n## D\ndd", 100, 10);
        let origins: Vec<&str> = cs.iter().map(|c| c.origin.as_str()).collect();
        assert_eq!(origins, vec!["› A", "› A › B", "› A › B › C", "› A › D"]);
        // The deepest chunk carries its ancestor path ("A › B") prefixed in-body.
        let c = cs.iter().find(|c| c.origin == "› A › B › C").unwrap();
        assert!(c.text.starts_with("A › B\n"));
        assert!(c.text.contains("cc"));
    }

    #[test]
    fn markdown_oversized_nested_prefixes_ancestors_on_each_part() {
        // Budget 4 words; ancestor prefix "A" costs 1, leaving 3 for content.
        let cs = markdown("# A\n## B\nw1 w2 w3 w4 w5 w6", 4, 1);
        let parts: Vec<&Chunk> = cs
            .iter()
            .filter(|c| c.origin.starts_with("› A › B"))
            .collect();
        assert!(parts.len() > 1, "nested section split into parts");
        for p in &parts {
            assert!(p.text.starts_with("A\n"), "ancestor prefixed on every part");
        }
    }

    #[test]
    fn markdown_various_heading_levels_and_title_trim() {
        let cs = markdown("######   Deep Title   \ntext", 100, 10);
        assert_eq!(cs[0].origin, "› Deep Title");
    }

    #[test]
    fn markdown_empty_and_whitespace_input() {
        assert!(markdown("", 5, 2).is_empty());
        assert!(markdown("   ", 5, 2).is_empty());
    }

    #[test]
    fn window_step_no_infinite_loop_when_overlap_ge_size() {
        // overlap >= size: step falls back to size, still terminates & covers text.
        let words = ["a", "b", "c", "d"];
        assert_eq!(window(&words, 2, 5), vec!["a b", "c d"]);
        // Normal overlap produces sliding windows.
        assert_eq!(window(&words, 2, 1), vec!["a b", "b c", "c d"]);
    }
}
