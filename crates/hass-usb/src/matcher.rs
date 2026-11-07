use crate::models::USBDevice;

/// One generated or custom Home Assistant USB matcher. `None` means the key is
/// absent; the required domain always contributes one specificity field.
#[allow(non_camel_case_types)]
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct USBMatcher {
    pub domain: String,
    pub vid: Option<String>,
    pub pid: Option<String>,
    pub serial_number: Option<String>,
    pub manufacturer: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct USBMatcherSpec {
    pub domain: &'static str,
    pub vid: Option<&'static str>,
    pub pid: Option<&'static str>,
    pub serial_number: Option<&'static str>,
    pub manufacturer: Option<&'static str>,
    pub description: Option<&'static str>,
}

impl USBMatcher {
    pub fn new(domain: impl Into<String>) -> Self {
        Self {
            domain: domain.into(),
            vid: None,
            pid: None,
            serial_number: None,
            manufacturer: None,
            description: None,
        }
    }

    pub fn specificity(&self) -> usize {
        1 + usize::from(self.vid.is_some())
            + usize::from(self.pid.is_some())
            + usize::from(self.serial_number.is_some())
            + usize::from(self.manufacturer.is_some())
            + usize::from(self.description.is_some())
    }
}

impl USBMatcherSpec {
    pub fn to_matcher(self) -> USBMatcher {
        USBMatcher {
            domain: self.domain.to_owned(),
            vid: self.vid.map(str::to_owned),
            pid: self.pid.map(str::to_owned),
            serial_number: self.serial_number.map(str::to_owned),
            manufacturer: self.manufacturer.map(str::to_owned),
            description: self.description.map(str::to_owned),
        }
    }
}

/// Return whether every supplied matcher field matches the device.
pub fn usb_device_matches_matcher(device: &USBDevice, matcher: &USBMatcher) -> bool {
    matcher
        .vid
        .as_ref()
        .is_none_or(|value| device.vid == *value)
        && matcher
            .pid
            .as_ref()
            .is_none_or(|value| device.pid == *value)
        && matcher
            .serial_number
            .as_deref()
            .is_none_or(|pattern| fnmatch_lower(device.serial_number.as_deref(), pattern))
        && matcher
            .manufacturer
            .as_deref()
            .is_none_or(|pattern| fnmatch_lower(device.manufacturer.as_deref(), pattern))
        && matcher
            .description
            .as_deref()
            .is_none_or(|pattern| fnmatch_lower(device.description.as_deref(), pattern))
}

/// Match generated core entries first and custom entries afterward, retaining
/// both source orderings. Hassfest's `known_devices` key is absent from this
/// runtime matcher type.
pub fn merge_usb_matchers(core: &[USBMatcher], custom: &[USBMatcher]) -> Vec<USBMatcher> {
    core.iter().chain(custom).cloned().collect()
}

/// Return every matcher at the greatest specificity, preserving source order
/// among ties. Specificity counts `domain` and every supplied field.
pub fn matching_usb_matchers<'a>(
    device: &USBDevice,
    matchers: &'a [USBMatcher],
) -> Vec<&'a USBMatcher> {
    let mut matched = matchers
        .iter()
        .filter(|matcher| usb_device_matches_matcher(device, matcher));
    let first = match matched.next() {
        Some(matcher) => matcher,
        None => return Vec::new(),
    };
    let mut most_specific = vec![first];
    let mut best = first.specificity();
    for matcher in matched {
        let specificity = matcher.specificity();
        match specificity.cmp(&best) {
            std::cmp::Ordering::Greater => {
                most_specific.clear();
                most_specific.push(matcher);
                best = specificity;
            }
            std::cmp::Ordering::Equal => most_specific.push(matcher),
            std::cmp::Ordering::Less => {}
        }
    }
    most_specific
}

#[derive(Clone, Debug)]
enum PatternToken {
    Literal(char),
    Any,
    Star,
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

fn fnmatch_lower(value: Option<&str>, pattern: &str) -> bool {
    let Some(value) = value else {
        return false;
    };
    let lowered: String = value.chars().flat_map(char::to_lowercase).collect();
    fnmatch(&lowered, pattern)
}

/// Python `fnmatch.fnmatch` behavior relevant to USB manifest patterns:
/// `*`, `?`, bracket classes/ranges, and a case-sensitive pattern. Path
/// separators are ordinary characters.
fn fnmatch(value: &str, pattern: &str) -> bool {
    let tokens = tokenize(pattern);
    let chars: Vec<char> = value.chars().collect();
    let mut memo = vec![vec![None; chars.len() + 1]; tokens.len() + 1];
    match_at(0, 0, &tokens, &chars, &mut memo)
}

fn match_at(
    token_index: usize,
    char_index: usize,
    tokens: &[PatternToken],
    chars: &[char],
    memo: &mut [Vec<Option<bool>>],
) -> bool {
    if let Some(result) = memo[token_index][char_index] {
        return result;
    }
    let result = if token_index == tokens.len() {
        char_index == chars.len()
    } else {
        match &tokens[token_index] {
            PatternToken::Literal(expected) => {
                chars.get(char_index) == Some(expected)
                    && match_at(token_index + 1, char_index + 1, tokens, chars, memo)
            }
            PatternToken::Any => {
                char_index < chars.len()
                    && match_at(token_index + 1, char_index + 1, tokens, chars, memo)
            }
            PatternToken::Star => {
                match_at(token_index + 1, char_index, tokens, chars, memo)
                    || (char_index < chars.len()
                        && match_at(token_index, char_index + 1, tokens, chars, memo))
            }
            PatternToken::Class { negated, ranges } => chars.get(char_index).is_some_and(|value| {
                let found = ranges
                    .iter()
                    .any(|(start, end)| start <= value && value <= end);
                (found != *negated)
                    && match_at(token_index + 1, char_index + 1, tokens, chars, memo)
            }),
        }
    };
    memo[token_index][char_index] = Some(result);
    result
}

fn tokenize(pattern: &str) -> Vec<PatternToken> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::with_capacity(chars.len());
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '*' => {
                if !matches!(tokens.last(), Some(PatternToken::Star)) {
                    tokens.push(PatternToken::Star);
                }
                index += 1;
            }
            '?' => {
                tokens.push(PatternToken::Any);
                index += 1;
            }
            '[' => match parse_class(&chars, index) {
                Some((token, next)) => {
                    tokens.push(token);
                    index = next;
                }
                None => {
                    tokens.push(PatternToken::Literal('['));
                    index += 1;
                }
            },
            literal => {
                tokens.push(PatternToken::Literal(literal));
                index += 1;
            }
        }
    }
    tokens
}

fn parse_class(chars: &[char], start: usize) -> Option<(PatternToken, usize)> {
    let mut cursor = start + 1;
    if cursor >= chars.len() {
        return None;
    }
    let negated = chars[cursor] == '!';
    if negated {
        cursor += 1;
    }
    let first = cursor;
    if chars.get(cursor) == Some(&']') {
        cursor += 1;
    }
    while cursor < chars.len() && chars[cursor] != ']' {
        cursor += 1;
    }
    if cursor == chars.len() {
        return None;
    }
    let end = cursor;
    let class_chars = &chars[first..end];
    let mut ranges = Vec::new();
    let mut position = 0;
    while position < class_chars.len() {
        if position + 2 < class_chars.len() && class_chars[position + 1] == '-' {
            let low = class_chars[position];
            let high = class_chars[position + 2];
            if low <= high {
                ranges.push((low, high));
            }
            position += 3;
        } else {
            ranges.push((class_chars[position], class_chars[position]));
            position += 1;
        }
    }
    Some((PatternToken::Class { negated, ranges }, end + 1))
}
