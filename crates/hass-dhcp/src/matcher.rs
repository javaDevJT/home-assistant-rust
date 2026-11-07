//! Ordered Home Assistant DHCP matcher metadata and Python `fnmatchcase` rules.
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
/// Pinned Home Assistant Core object used for the bundled generated list.
pub const GENERATED_SOURCE_COMMIT: &str = "cbf2c02c9bff331537c19bff69da8dc11c3fbfbf";
/// One generated or custom component DHCP matcher, retaining its source fields.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DhcpMatcher {
    /// Integration domain that owns a matching DHCP device.
    pub domain: String,
    /// Optional case-sensitive Python `fnmatchcase` hostname pattern.
    #[serde(default)]
    pub hostname: Option<String>,
    /// Optional source MAC prefix pattern, commonly ending in `*`.
    #[serde(default)]
    pub macaddress: Option<String>,
    /// Presence selects registered-device matching, including an explicit
    /// false value, as in the pinned loader/indexer contract.
    #[serde(default, deserialize_with = "deserialize_registered_devices")]
    pub registered_devices: Option<bool>,
}

fn deserialize_registered_devices<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    bool::deserialize(deserializer).map(Some)
}
/// Source-ordered matchers indexed the same way as Home Assistant Core.
#[derive(Clone, Debug, Default)]
pub struct DhcpMatcherIndex {
    matchers: Vec<DhcpMatcher>,
    registered_devices_domains: HashSet<String>,
    no_oui_matchers: HashMap<String, Vec<usize>>,
    oui_matchers: HashMap<String, Vec<usize>>,
}
impl DhcpMatcherIndex {
    /// Build an index without reordering matcher metadata.
    pub fn new(matchers: Vec<DhcpMatcher>) -> Self {
        let mut index = Self {
            matchers,
            ..Self::default()
        };
        for (matcher_index, matcher) in index.matchers.iter().enumerate() {
            if matcher.registered_devices.is_some() {
                index
                    .registered_devices_domains
                    .insert(matcher.domain.clone());
                continue;
            }
            if let Some(macaddress) = matcher
                .macaddress
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                index
                    .oui_matchers
                    .entry(first_chars(macaddress, 6))
                    .or_default()
                    .push(matcher_index);
                continue;
            }
            if let Some(hostname) = matcher
                .hostname
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                index
                    .no_oui_matchers
                    .entry(first_lower_character(hostname))
                    .or_default()
                    .push(matcher_index);
            }
        }
        index
    }
    /// Load the exact generated DHCP matcher list from the pinned source.
    pub fn generated() -> Self {
        let matchers = serde_json::from_str(include_str!("generated/dhcp_matchers.json"))
            .expect("bundled generated DHCP matcher metadata must be valid JSON");
        Self::new(matchers)
    }
    /// Load generated matchers and append runtime custom-component matchers.
    pub fn generated_with_custom(custom_matchers: impl IntoIterator<Item = DhcpMatcher>) -> Self {
        let mut matchers = Self::generated().matchers;
        matchers.extend(custom_matchers);
        Self::new(matchers)
    }
    /// Append custom-component matchers after the current source-ordered list.
    pub fn with_custom(mut self, custom_matchers: impl IntoIterator<Item = DhcpMatcher>) -> Self {
        self.matchers.extend(custom_matchers);
        Self::new(self.matchers)
    }
    /// Return generated and custom matchers in their original append order.
    pub fn matchers(&self) -> &[DhcpMatcher] {
        &self.matchers
    }
    /// Return every target domain in deterministic first-match order.
    pub(crate) fn matching_domains(
        &self,
        lowercase_hostname: &str,
        uppercase_mac: &str,
        registered_domains: impl IntoIterator<Item = String>,
    ) -> Vec<String> {
        let mut domains = Vec::new();
        let mut seen = HashSet::new();
        for domain in registered_domains {
            if self.registered_devices_domains.contains(&domain) && seen.insert(domain.clone()) {
                domains.push(domain);
            }
        }
        let hostname_key = lowercase_hostname
            .chars()
            .next()
            .map(|character| character.to_string())
            .unwrap_or_default();
        let oui_key = first_chars(uppercase_mac, 6);
        let no_oui = self
            .no_oui_matchers
            .get(&hostname_key)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let oui = self
            .oui_matchers
            .get(&oui_key)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for matcher_index in no_oui.iter().chain(oui) {
            let matcher = &self.matchers[*matcher_index];
            if let Some(pattern) = matcher.hostname.as_deref()
                && !python_fnmatchcase(lowercase_hostname, pattern)
            {
                continue;
            }
            if seen.insert(matcher.domain.clone()) {
                domains.push(matcher.domain.clone());
            }
        }
        domains
    }
}
fn first_chars(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}
fn first_lower_character(value: &str) -> String {
    value
        .chars()
        .next()
        .map(|character| character.to_lowercase().collect())
        .unwrap_or_default()
}
/// Case-sensitive shell matching equivalent to Python's `fnmatchcase`.
///
/// `*`, `?`, `[seq]`, `[!seq]`, ranges, and unmatched opening brackets follow
/// Python's `fnmatch.translate` behavior. Backslashes are ordinary characters;
/// wildcard characters match newlines as they do in the source regex.
fn python_fnmatchcase(candidate: &str, pattern: &str) -> bool {
    let tokens = parse_pattern(pattern);
    let characters: Vec<char> = candidate.chars().collect();
    let mut matched = vec![vec![false; characters.len() + 1]; tokens.len() + 1];
    matched[tokens.len()][characters.len()] = true;
    for token_index in (0..tokens.len()).rev() {
        match &tokens[token_index] {
            PatternToken::AnySequence => {
                matched[token_index][characters.len()] = matched[token_index + 1][characters.len()];
                for character_index in (0..characters.len()).rev() {
                    matched[token_index][character_index] = matched[token_index + 1]
                        [character_index]
                        || matched[token_index][character_index + 1];
                }
            }
            PatternToken::AnyCharacter => {
                for character_index in 0..characters.len() {
                    matched[token_index][character_index] =
                        matched[token_index + 1][character_index + 1];
                }
            }
            PatternToken::Literal(expected) => {
                for character_index in 0..characters.len() {
                    matched[token_index][character_index] = characters[character_index]
                        == *expected
                        && matched[token_index + 1][character_index + 1];
                }
            }
            PatternToken::CharacterClass { negated, members } => {
                for character_index in 0..characters.len() {
                    let contained = members.iter().any(|member| match member {
                        ClassMember::Literal(value) => characters[character_index] == *value,
                        ClassMember::Range(start, end) => {
                            *start <= characters[character_index]
                                && characters[character_index] <= *end
                        }
                    });
                    matched[token_index][character_index] =
                        (contained != *negated) && matched[token_index + 1][character_index + 1];
                }
            }
            PatternToken::Never => {}
        }
    }
    matched[0][0]
}
#[derive(Clone, Debug)]
enum PatternToken {
    AnySequence,
    AnyCharacter,
    Literal(char),
    CharacterClass {
        negated: bool,
        members: Vec<ClassMember>,
    },
    Never,
}
#[derive(Clone, Debug)]
enum ClassMember {
    Literal(char),
    Range(char, char),
}
#[derive(Clone, Copy)]
struct ClassCharacter {
    value: char,
    escaped: bool,
}
fn parse_pattern(pattern: &str) -> Vec<PatternToken> {
    let characters: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        match characters[index] {
            '*' => {
                if !matches!(tokens.last(), Some(PatternToken::AnySequence)) {
                    tokens.push(PatternToken::AnySequence);
                }
                index += 1;
            }
            '?' => {
                tokens.push(PatternToken::AnyCharacter);
                index += 1;
            }
            '[' => {
                if let Some((token, next_index)) = parse_character_class(&characters, index) {
                    tokens.push(token);
                    index = next_index;
                } else {
                    tokens.push(PatternToken::Literal('['));
                    index += 1;
                }
            }
            literal => {
                tokens.push(PatternToken::Literal(literal));
                index += 1;
            }
        }
    }
    tokens
}
fn parse_character_class(pattern: &[char], open_index: usize) -> Option<(PatternToken, usize)> {
    let mut close_index = open_index + 1;
    if close_index < pattern.len() && pattern[close_index] == '!' {
        close_index += 1;
    }
    if close_index < pattern.len() && pattern[close_index] == ']' {
        close_index += 1;
    }
    while close_index < pattern.len() && pattern[close_index] != ']' {
        close_index += 1;
    }
    if close_index == pattern.len() {
        return None;
    }
    let raw_members = &pattern[open_index + 1..close_index];
    let negated = raw_members.first() == Some(&'!');
    let members = parse_class_members(raw_members, negated);
    let token = if members.is_empty() {
        if negated {
            PatternToken::AnyCharacter
        } else {
            PatternToken::Never
        }
    } else {
        PatternToken::CharacterClass { negated, members }
    };
    Some((token, close_index + 1))
}
fn parse_class_members(raw_members: &[char], negated: bool) -> Vec<ClassMember> {
    // Adapted from the sibling `hass-zeroconf` matcher port so both native
    // discovery SDKs share the same Python class and range behavior.
    let mut chunks = Vec::<Vec<char>>::new();
    let mut chunk_start = 0;
    let mut search_from = usize::from(negated) + 1;
    while let Some(relative_index) = raw_members[search_from..]
        .iter()
        .position(|character| *character == '-')
    {
        let range_separator = search_from + relative_index;
        chunks.push(raw_members[chunk_start..range_separator].to_vec());
        chunk_start = range_separator + 1;
        search_from = range_separator + 3;
        if search_from > raw_members.len() {
            break;
        }
    }
    let last_chunk = raw_members[chunk_start..].to_vec();
    if !last_chunk.is_empty() {
        chunks.push(last_chunk);
    } else if let Some(last_chunk) = chunks.last_mut() {
        last_chunk.push('-');
    }
    // Python removes descending ranges, joining the remaining class chunks.
    for index in (1..chunks.len()).rev() {
        let range_start = chunks[index - 1].last().copied();
        let range_end = chunks[index].first().copied();
        if matches!((range_start, range_end), (Some(start), Some(end)) if start > end) {
            let mut merged = chunks[index - 1][..chunks[index - 1].len() - 1].to_vec();
            merged.extend_from_slice(&chunks[index][1..]);
            chunks[index - 1] = merged;
            chunks.remove(index);
        }
    }
    let mut translated = Vec::<ClassCharacter>::new();
    for (index, chunk) in chunks.iter().enumerate() {
        translated.extend(chunk.iter().copied().map(|value| ClassCharacter {
            value,
            escaped: value == '\\' || value == '-',
        }));
        if index + 1 < chunks.len() {
            translated.push(ClassCharacter {
                value: '-',
                escaped: false,
            });
        }
    }
    if negated && translated.first().map(|character| character.value) == Some('!') {
        translated.remove(0);
    }
    let mut members = Vec::with_capacity(raw_members.len());
    let mut index = 0;
    while index < translated.len() {
        if index + 2 < translated.len()
            && translated[index + 1].value == '-'
            && !translated[index + 1].escaped
        {
            let start = translated[index].value;
            let end = translated[index + 2].value;
            if start <= end {
                members.push(ClassMember::Range(start, end));
            }
            index += 3;
        } else {
            members.push(ClassMember::Literal(translated[index].value));
            index += 1;
        }
    }
    members
}
#[cfg(test)]
mod tests {
    use super::{DhcpMatcherIndex, GENERATED_SOURCE_COMMIT, python_fnmatchcase};
    #[test]
    fn generated_matcher_snapshot_retains_pinned_order_and_metadata() {
        let index = DhcpMatcherIndex::generated();
        assert_eq!(
            GENERATED_SOURCE_COMMIT,
            "cbf2c02c9bff331537c19bff69da8dc11c3fbfbf"
        );
        assert_eq!(index.matchers().len(), 386);
        assert_eq!(
            index
                .matchers()
                .first()
                .map(|matcher| matcher.domain.as_str()),
            Some("actron_air")
        );
        assert_eq!(
            index
                .matchers()
                .last()
                .map(|matcher| matcher.domain.as_str()),
            Some("yoto")
        );
        assert_eq!(
            index
                .matchers()
                .iter()
                .filter(|matcher| matcher.registered_devices.is_some())
                .count(),
            43
        );
        assert_eq!(index.matchers()[1].hostname.as_deref(), Some("aidot"));
    }
    #[test]
    fn registered_device_matching_depends_on_key_presence_including_false() {
        let matcher: super::DhcpMatcher = serde_json::from_value(serde_json::json!({
            "domain": "registered_false",
            "registered_devices": false,
            "hostname": "unit-*",
            "macaddress": "ABCDEF*"
        }))
        .unwrap();
        assert_eq!(matcher.registered_devices, Some(false));
        let index = DhcpMatcherIndex::new(vec![matcher]);
        assert!(
            index
                .matching_domains("unit-one", "ABCDEF000000", Vec::<String>::new())
                .is_empty()
        );
        assert_eq!(
            index.matching_domains("other", "123456000000", vec!["registered_false".to_owned()],),
            vec!["registered_false".to_owned()]
        );
        assert!(
            serde_json::from_value::<super::DhcpMatcher>(serde_json::json!({
                "domain": "invalid",
                "registered_devices": null
            }))
            .is_err()
        );
    }

    #[test]
    fn hostname_patterns_follow_python_fnmatchcase_semantics() {
        assert!(python_fnmatchcase("klp100", "k[lps]*"));
        assert!(!python_fnmatchcase("ktp100", "k[lps]*"));
        assert!(python_fnmatchcase("node-7", "node-[0-9]"));
        assert!(python_fnmatchcase("bravo", "[!a]*"));
        assert!(!python_fnmatchcase("apple", "[!a]*"));
        assert!(python_fnmatchcase("abc[", "abc["));
        assert!(python_fnmatchcase("[]", "[]"));
        assert!(python_fnmatchcase("a\nb", "a*b"));
        assert!(python_fnmatchcase("[!]", "[!]"));
        assert!(python_fnmatchcase("x", "[!z-a]"));
        assert!(!python_fnmatchcase("", "[!z-a]"));
        assert!(!python_fnmatchcase("m", "[z-a]"));
        assert!(!python_fnmatchcase("", "?"));
        assert!(python_fnmatchcase("\\*", "\\*"));
    }
}
