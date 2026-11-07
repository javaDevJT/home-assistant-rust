//! Home Assistant application-credential IDs use `homeassistant.util.slugify`
//! with its default separator (`_`).

use std::collections::HashMap;
use std::sync::OnceLock;

use unicode_normalization::UnicodeNormalization;

const TEXT_UNIDECODE_DATA: &[u8] = include_bytes!("../data/text-unidecode-1.3.bin");
const HTML4_ENTITIES: &str = include_str!("../data/html4-name2codepoint.tsv");
const UNICODE16_DECIMAL_ZEROES: &str = include_str!("../data/unicode16-decimal-zeroes.txt");
const TRANSLITERATION_BLOCK_SIZE: usize = 256;
const TEXT_UNIDECODE_CODEPOINTS: usize = 0xFFFF;

static TRANSLITERATION_BLOCK_OFFSETS: OnceLock<Vec<usize>> = OnceLock::new();
static HTML4_ENTITY_MAP: OnceLock<HashMap<&'static str, char>> = OnceLock::new();

/// Create the underscore-separated ID used by Home Assistant's slugify helper.
///
/// Empty input stays empty. Non-empty input that reduces to no ASCII slug uses
/// Home Assistant's `"unknown"` fallback.
pub(crate) fn slugify_id(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }

    // python-slugify's legacy pipeline replaces apostrophes before Unicode
    // normalization and transliteration.
    let quoted = input.replace('\'', "-");
    let normalized = quoted.nfkd().collect::<String>();
    let transliterated = transliterate(&normalized);

    // Entity decoding follows transliteration in python-slugify 9.1.3's frozen
    // legacy pipeline. The pass order matters for nested references.
    let named = decode_named_entities(&transliterated);
    let decimal = decode_numeric_entities(&named, false);
    let hexadecimal = decode_numeric_entities(&decimal, true);
    let normalized = hexadecimal.nfkd().collect::<String>();
    let lowercase = normalized.to_lowercase();
    let unquoted = lowercase.replace('\'', "");
    let without_numeric_commas = remove_numeric_commas(&unquoted);
    let slug = ascii_slug(&without_numeric_commas);

    if slug.is_empty() {
        "unknown".to_owned()
    } else {
        slug.replace('-', "_")
    }
}

fn transliterate(text: &str) -> String {
    // This is python-slugify's ASCII fast path. The bundled table only needs
    // to be consulted when at least one non-ASCII code point is present.
    if text.is_ascii() {
        return text.to_owned();
    }

    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '\0' {
            output.push('\0');
        } else if let Some(replacement) = text_unidecode(character as u32) {
            output.push_str(replacement);
        }
    }
    output
}

fn text_unidecode(codepoint: u32) -> Option<&'static str> {
    if codepoint == 0 {
        return Some("\0");
    }

    let index = codepoint as usize - 1;
    if index >= TEXT_UNIDECODE_CODEPOINTS {
        // text-unidecode 1.3's table covers the BMP; its implementation drops
        // characters whose table index is out of range.
        return None;
    }

    let block_index = index / TRANSLITERATION_BLOCK_SIZE;
    let in_block = index % TRANSLITERATION_BLOCK_SIZE;
    let block_offsets = TRANSLITERATION_BLOCK_OFFSETS.get_or_init(|| {
        let mut offsets =
            Vec::with_capacity(TEXT_UNIDECODE_CODEPOINTS.div_ceil(TRANSLITERATION_BLOCK_SIZE));
        offsets.push(0);

        let mut entry_index = 0;
        for (byte_index, byte) in TEXT_UNIDECODE_DATA.iter().enumerate() {
            if *byte == 0 {
                entry_index += 1;
                if entry_index % TRANSLITERATION_BLOCK_SIZE == 0 {
                    offsets.push(byte_index + 1);
                }
            }
        }
        offsets
    });

    let mut start = *block_offsets.get(block_index)?;
    for _ in 0..in_block {
        let delimiter = TEXT_UNIDECODE_DATA[start..]
            .iter()
            .position(|byte| *byte == 0)?;
        start += delimiter + 1;
    }

    let end = TEXT_UNIDECODE_DATA[start..]
        .iter()
        .position(|byte| *byte == 0)
        .map_or(TEXT_UNIDECODE_DATA.len(), |delimiter| start + delimiter);
    std::str::from_utf8(&TEXT_UNIDECODE_DATA[start..end]).ok()
}

fn html4_entity(name: &str) -> Option<char> {
    let entities = HTML4_ENTITY_MAP.get_or_init(|| {
        HTML4_ENTITIES
            .lines()
            .filter_map(|line| {
                let (name, codepoint) = line.split_once('\t')?;
                let codepoint = u32::from_str_radix(codepoint, 16).ok()?;
                Some((name, char::from_u32(codepoint)?))
            })
            .collect()
    });
    entities.get(name).copied()
}

fn decode_named_entities(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    while cursor < text.len() {
        if text.as_bytes()[cursor] == b'&'
            && let Some(relative_end) = text[cursor + 1..].find(';')
        {
            let end = cursor + 1 + relative_end;
            if let Some(character) = html4_entity(&text[cursor + 1..end]) {
                output.push(character);
                cursor = end + 1;
                continue;
            }
        }

        let character = text[cursor..]
            .chars()
            .next()
            .expect("cursor is before the end of the string");
        output.push(character);
        cursor += character.len_utf8();
    }
    output
}

fn decode_numeric_entities(text: &str, hexadecimal: bool) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;

    while cursor < text.len() {
        if bytes[cursor] == b'&' && bytes.get(cursor + 1) == Some(&b'#') {
            let mut digits_start = cursor + 2;
            if hexadecimal {
                match bytes.get(digits_start) {
                    Some(b'x') => digits_start += 1,
                    _ => {
                        cursor = push_next_character(text, cursor, &mut output);
                        continue;
                    }
                }
            }

            let radix = if hexadecimal { 16 } else { 10 };
            let mut position = digits_start;
            let mut digit_count = 0;
            let mut value = 0_u32;
            let mut overflowed = false;

            while position < text.len() {
                let character = text[position..]
                    .chars()
                    .next()
                    .expect("position is before the end of the string");
                let digit = if hexadecimal {
                    decimal_digit_value(character).or_else(|| character.to_digit(radix))
                } else {
                    decimal_digit_value(character)
                };
                let Some(digit) = digit.filter(|digit| *digit < radix) else {
                    break;
                };

                if !overflowed {
                    if let Some(next) = value.checked_mul(radix).and_then(|n| n.checked_add(digit))
                    {
                        value = next;
                    } else {
                        overflowed = true;
                    }
                }
                digit_count += 1;
                position += character.len_utf8();
            }

            if digit_count > 0 && bytes.get(position) == Some(&b';') {
                let end = position + 1;
                if overflowed || (!hexadecimal && digit_count > 4_300) || value > 0x10_FFFF {
                    // The pinned Python implementation catches conversion
                    // errors around the whole regex substitution. Returning
                    // the original text rolls back any earlier references.
                    return text.to_owned();
                }

                // Python chr() accepts surrogate values, but Rust `char`
                // cannot represent them. A NUL sentinel has the same later
                // slug-filter result (one separator) without retaining digits.
                if (0xD800..=0xDFFF).contains(&value) {
                    output.push('\0');
                } else if let Some(character) = char::from_u32(value) {
                    output.push(character);
                }
                cursor = end;
                continue;
            }
        }

        cursor = push_next_character(text, cursor, &mut output);
    }

    output
}

fn push_next_character(text: &str, cursor: usize, output: &mut String) -> usize {
    let character = text[cursor..]
        .chars()
        .next()
        .expect("cursor is before the end of the string");
    output.push(character);
    cursor + character.len_utf8()
}

fn decimal_digit_value(character: char) -> Option<u32> {
    static ZEROES: OnceLock<Vec<u32>> = OnceLock::new();
    let zeroes = ZEROES.get_or_init(|| {
        UNICODE16_DECIMAL_ZEROES
            .lines()
            .filter_map(|line| u32::from_str_radix(line, 16).ok())
            .collect()
    });

    let codepoint = character as u32;
    for zero in zeroes {
        let zero = *zero;
        if codepoint >= zero && codepoint < zero + 10 {
            return Some(codepoint - zero);
        }
    }
    None
}

fn remove_numeric_commas(text: &str) -> String {
    let characters = text.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(text.len());
    for (index, character) in characters.iter().enumerate() {
        let thousands_separator = *character == ','
            && index > 0
            && index + 1 < characters.len()
            && decimal_digit_value(characters[index - 1]).is_some()
            && decimal_digit_value(characters[index + 1]).is_some();
        if !thousands_separator {
            output.push(*character);
        }
    }
    output
}

fn ascii_slug(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character);
        } else if output.ends_with('-') {
            continue;
        } else {
            // The pinned Python pattern replaces each run of characters other
            // than ASCII letters, digits, or hyphens with one hyphen.
            output.push('-');
        }
    }
    output.trim_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use super::slugify_id;

    #[test]
    fn keeps_empty_input_and_uses_unknown_for_empty_slugs() {
        assert_eq!(slugify_id(""), "");
        assert_eq!(slugify_id("!!!"), "unknown");
        assert_eq!(slugify_id("😀"), "unknown");
    }

    #[test]
    fn transliterates_latin_cjk_and_greek_with_the_pinned_table() {
        assert_eq!(slugify_id("café"), "cafe");
        assert_eq!(slugify_id("汉字"), "yi_zi");
        assert_eq!(slugify_id("κόσμος"), "kosmos");
    }

    #[test]
    fn decodes_named_decimal_and_hex_entities_after_transliteration() {
        assert_eq!(slugify_id("a&amp;b"), "a_b");
        assert_eq!(slugify_id("a&#38;b"), "a_b");
        assert_eq!(slugify_id("a&#x26;b"), "a_b");
        assert_eq!(slugify_id("a&#X26;b"), "a_x26_b");
        assert_eq!(slugify_id("caf&#233;x"), "cafe_x");
        assert_eq!(slugify_id("caféx"), "cafex");
        assert_eq!(slugify_id("a&AMP;b"), "a_amp_b");
        assert_eq!(slugify_id("a&apos;b"), "a_apos_b");
        assert_eq!(slugify_id("a&#55296;b"), "a_b");
    }

    #[test]
    fn numeric_reference_passes_roll_back_on_conversion_errors() {
        use super::decode_numeric_entities;

        assert_eq!(
            decode_numeric_entities("&#65; &#1114112; &#66;", false),
            "&#65; &#1114112; &#66;"
        );
        assert_eq!(
            decode_numeric_entities("&#x41; &#x110000; &#x42;", true),
            "&#x41; &#x110000; &#x42;"
        );
        assert_eq!(decode_numeric_entities("&#XD800;", true), "&#XD800;");

        let oversized_decimal = format!("&#{}; &#65;", "0".repeat(4_301));
        assert_eq!(
            decode_numeric_entities(&oversized_decimal, false),
            oversized_decimal
        );
    }

    #[test]
    fn follows_ascii_delimiters_and_default_unbounded_length() {
        assert_eq!(slugify_id("OAuth.Client / 7,001"), "oauth_client_7001");
        assert_eq!(slugify_id("User's ID"), "user_s_id");
        assert_eq!(slugify_id("&#49;,&#50;"), "12");

        let long_id = "a".repeat(300);
        assert_eq!(slugify_id(&long_id), long_id);
    }
}
