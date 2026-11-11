// crates/audiolib-db/src/search_text.rs
//! Shared Chinese segmentation and Unicode word boundaries for FTS5 input.
use audiolib_core::{Error, Result};
use icu_normalizer::{DecomposingNormalizer, properties::CanonicalCombiningClassMap};
use jieba_rs::Jieba;
use std::fmt::Write;
use unicode_segmentation::UnicodeSegmentation;

const RAW_COLUMNS: &str = "{filename user_tags user_notes bwf_description ixml_scene ixml_take}";
const DERIVED_COLUMNS: &str = "{filename_tokenized user_tags_tokenized user_notes_tokenized bwf_description_tokenized ixml_scene_tokenized ixml_take_tokenized}";

fn has_letters(word: &str) -> bool {
    word.chars()
        .any(|character| character.is_alphabetic() && !is_han(character))
}

fn searchable_grapheme_count(word: &str) -> usize {
    word.graphemes(true)
        .filter(|grapheme| grapheme.chars().any(char::is_alphanumeric))
        .count()
}

// Encoded tokens cannot collide with raw Han/numeric derived
// terms. Fixed-width scalar pairs preserve punctuation without exposing FTS
// syntax. Positions, rather than an unordered AND of grams, enforce continuity.
fn normalized_letters(word: &str) -> Vec<char> {
    let folded = icu_casemap::CaseMapper::new().fold_string(word);
    let classes = CanonicalCombiningClassMap::new();
    DecomposingNormalizer::new_nfd()
        .normalize_iter(folded.chars())
        .filter(|character| classes.get_u8(*character) == 0)
        .map(|character| if character == '_' { '.' } else { character })
        .collect()
}

fn letter_bigrams(word: &str) -> Option<String> {
    if searchable_grapheme_count(word) < 2 {
        return None;
    }
    normalized_bigrams(word)
}

fn normalized_bigrams(word: &str) -> Option<String> {
    if !has_letters(word) {
        return None;
    }
    let normalized = normalized_letters(word);
    if normalized.len() < 2 {
        return None;
    }
    let mut text = String::new();
    for pair in normalized.windows(2) {
        if !text.is_empty() {
            text.push(' ');
        }
        // Writing into a String is infallible.
        write!(text, "g{:06x}{:06x}", pair[0] as u32, pair[1] as u32).unwrap();
    }
    Some(text)
}

fn single_letter_token(word: &str) -> String {
    let mut text = String::from("s");
    for character in normalized_letters(word) {
        write!(text, "{:06x}", character as u32).unwrap();
    }
    text
}

// unicode61 treats dot/underscore compounds as separate tokens. Keep their
// numeric and single-letter components searchable even when source Metadata
// exists only in a derived column. Guards retain intervening letter positions
// so, for example, 1_AB_2 cannot become the numeric phrase "1 2".
fn compound_exact_tokens(word: &str) -> Option<String> {
    if !has_letters(word) || !word.contains(['.', '_']) {
        return None;
    }
    let mut retained = false;
    let tokens: Vec<_> = word
        .split(['.', '_'])
        .filter(|part| part.chars().any(char::is_alphanumeric))
        .map(|part| {
            if !has_letters(part) {
                retained = true;
                part.to_owned()
            } else if searchable_grapheme_count(part) < 2 {
                retained = true;
                single_letter_token(part)
            } else {
                "z".to_owned()
            }
        })
        .collect();
    retained.then(|| tokens.join(" "))
}

// Split ideographic runs before applying Unicode word boundaries: UAX #29 gives
// individual Han characters, whereas the audio dictionary must keep Chinese words.
fn is_han(character: char) -> bool {
    matches!(character,
        '\u{3400}'..='\u{4dbf}'
        | '\u{4e00}'..='\u{9fff}'
        | '\u{f900}'..='\u{faff}'
        | '\u{20000}'..='\u{2ffff}'
        | '\u{30000}'..='\u{323af}'
    )
}

fn words<'a>(jieba: &Jieba, value: &'a str, for_search: bool) -> Vec<&'a str> {
    let mut words = Vec::new();
    let mut start = 0;
    let mut han = value.chars().next().is_some_and(is_han);
    let mut append = |part: &'a str, han: bool| {
        if han {
            let tokens = if for_search {
                jieba.cut_for_search(part, true)
            } else {
                jieba.cut(part, true)
            };
            words.extend(tokens.into_iter().map(|token| token.word));
        } else {
            // Keep accented letters and combining marks in their Unicode word.
            // SQLite's unicode61 tokenizer performs the case/diacritic folding;
            // splitting café into caf + é would change both recall and AND semantics.
            words.extend(part.unicode_words());
        }
    };
    for (offset, character) in value.char_indices() {
        let next_han = is_han(character);
        if next_han != han {
            append(&value[start..offset], han);
            start = offset;
            han = next_han;
        }
    }
    append(&value[start..], han);
    words
}

/// Index letter fragments as positional bigrams with linear storage/work.
/// Word boundaries prevent phrase matches joining unrelated words; Chinese
/// search-mode expansion and numeric tokens retain their previous semantics.
pub(crate) fn index_text(jieba: &Jieba, value: &str) -> String {
    words(jieba, value, true)
        .into_iter()
        .flat_map(query_word_parts)
        .map(|word| {
            // Input length restricts queries, not indexed normalized text. A
            // single grapheme such as ß or ﬃ may fold into multiple letters.
            let grams = normalized_bigrams(word);
            let keep_single =
                grams.is_some() && has_letters(word) && searchable_grapheme_count(word) < 2;
            let mut text = grams.map(|grams| format!("{grams} z")).unwrap_or_else(|| {
                if has_letters(word) {
                    single_letter_token(word)
                } else {
                    word.to_owned()
                }
            });
            if keep_single {
                // Preserve exact single-grapheme terms in multi-word queries,
                // and keep both encodings away from the next word's positions.
                text.push(' ');
                text.push_str(&single_letter_token(word));
                text.push_str(" z");
            }
            if let Some(tokens) = compound_exact_tokens(word) {
                text.push_str(" z ");
                text.push_str(&tokens);
            }
            text
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn query_word_parts(word: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    for (offset, grapheme) in word.grapheme_indices(true) {
        // UAX word boundaries keep internal colons, apostrophes and numeric
        // commas, while unicode61 splits them. Quoting the complete UAX word
        // would accidentally require adjacent FTS tokens instead of AND terms.
        // Keep letter/number graphemes intact, including their combining marks.
        // ASCII dot/underscore retain the existing compound/decimal phrases.
        if matches!(grapheme, "." | "_") || grapheme.chars().any(char::is_alphanumeric) {
            continue;
        }
        if start < offset {
            parts.push(&word[start..offset]);
        }
        start = offset + grapheme.len();
    }
    if start < word.len() {
        parts.push(&word[start..]);
    }
    parts
}

/// Compile plain-text AND terms with contiguous letter-fragment matching.
/// Two or more characters use an indexed bigram phrase; single letters in a
/// multi-term query, Han dictionary terms and numbers retain exact matching.
fn term_words<'a>(jieba: &'a Jieba, value: &'a str) -> Vec<&'a str> {
    words(jieba, value, false)
        .into_iter()
        .flat_map(query_word_parts)
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .collect()
}

/// Words of a query in the same order `compile_query` ANDs them.
pub(crate) fn query_terms(jieba: &Jieba, value: &str) -> Vec<String> {
    term_words(jieba, value)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// Compile plain-text AND terms against one raw/derived column pair.
/// `raw` and `derived` are fixed FTS column-filter literals, never request text.
pub(crate) fn compile_query_columns(
    jieba: &Jieba,
    value: &str,
    raw: &str,
    derived: &str,
) -> Result<String> {
    let terms: Vec<_> = term_words(jieba, value)
        .into_iter()
        .map(|word| {
            let quoted = word.replace('"', "\"\"");
            if let Some(grams) = letter_bigrams(word) {
                // Only original columns may satisfy the raw-word alternative:
                // a user code resembling an encoded gram must remain literal.
                format!("({raw} : \"{quoted}\" OR {derived} : \"{grams}\")")
            } else if has_letters(word) {
                format!(
                    "({raw} : \"{quoted}\" OR {derived} : \"{}\")",
                    single_letter_token(word)
                )
            } else {
                format!("\"{quoted}\"")
            }
        })
        .collect();
    if terms.is_empty() {
        return Err(Error::Validation("Search requires a word or number".into()));
    }
    Ok(terms.join(" AND "))
}

pub(crate) fn compile_query(jieba: &Jieba, value: &str) -> Result<String> {
    compile_query_columns(jieba, value, RAW_COLUMNS, DERIVED_COLUMNS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_fragments_fold_case_accents_and_combining_marks() {
        let jieba = Jieba::new();
        for (word, equivalent) in [
            ("café", "CAFE"),
            ("Cafe\u{301}", "cafe"),
            ("naïve", "naive"),
            ("Äpfel", "apfel"),
            ("Ελληνικά", "ΕΛΛΗΝΙΚΑ"),
        ] {
            assert_eq!(letter_bigrams(word), letter_bigrams(equivalent));
            assert_eq!(index_text(&jieba, word), index_text(&jieba, equivalent));
        }
        assert_eq!(
            letter_bigrams("plos").unwrap(),
            "g00007000006c g00006c00006f g00006f000073"
        );
        assert!(letter_bigrams("e").is_none());
    }

    #[test]
    fn punctuation_and_operator_names_remain_plain_and_terms() {
        let jieba = Jieba::new();
        let query = compile_query(&jieba, "metal (OR) \"wood\"*").unwrap();
        assert_eq!(query.matches(" AND ").count(), 2);
        assert!(query.contains(" : \"OR\" OR "));
        assert!(!query.contains('*'));
        assert!(compile_query(&jieba, "\" () *").is_err());
        assert_eq!(
            compile_query(&jieba, "door,slam")
                .unwrap()
                .matches(" AND ")
                .count(),
            1
        );
    }

    #[test]
    fn chinese_dictionary_words_and_search_expansions_are_retained() {
        let jieba = crate::load_tokenizer();
        assert!(
            compile_query(&jieba, "café爆炸声音")
                .unwrap()
                .ends_with(" AND \"爆炸\" AND \"声音\"")
        );
        let expected = jieba
            .cut_for_search("数字音频工作站", true)
            .into_iter()
            .map(|token| token.word)
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(index_text(&jieba, "数字音频工作站"), expected);
    }

    #[test]
    fn internal_uax_punctuation_keeps_and_terms_and_legacy_compound_phrases() {
        let jieba = Jieba::new();
        for query in ["foo:bar", "foo·bar", "foo：bar", "foo‿bar"] {
            assert_eq!(
                compile_query(&jieba, query).unwrap(),
                compile_query(&jieba, "foo bar").unwrap()
            );
        }
        for query in ["don't", "don’t", "don‘t", "don＇t"] {
            assert_eq!(
                compile_query(&jieba, query).unwrap(),
                compile_query(&jieba, "don t").unwrap()
            );
        }
        assert_eq!(compile_query(&jieba, "1,2").unwrap(), "\"1\" AND \"2\"");
        assert_eq!(
            compile_query(&jieba, "Cafe\u{301}:door").unwrap(),
            compile_query(&jieba, "Cafe\u{301} door").unwrap()
        );
        for query in ["foo.bar", "foo_bar"] {
            assert_eq!(
                compile_query(&jieba, query).unwrap(),
                format!(
                    "({RAW_COLUMNS} : \"{query}\" OR {DERIVED_COLUMNS} : \"{}\")",
                    letter_bigrams(query).unwrap()
                )
            );
        }
        assert_eq!(compile_query(&jieba, "1.2").unwrap(), "\"1.2\"");
        assert_ne!(index_text(&jieba, "foo bar"), index_text(&jieba, "foobar"));
        assert_eq!(letter_bigrams("foo.bar"), letter_bigrams("foo_bar"));
    }
}
