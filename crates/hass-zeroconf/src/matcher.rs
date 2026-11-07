//! Home Assistant's source-compatible Zeroconf matcher registry and rules.

use crate::models::ZeroconfServiceInfo;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::{BTreeMap, HashMap};
use std::fmt;

/// One generated or manifest-style service matcher.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ZeroconfMatcher {
    /// Integration domain associated with the matching service.
    pub domain: String,
    /// Optional case-sensitive Python fnmatch pattern for a lowercased name.
    #[serde(default)]
    pub name: Option<String>,
    /// Optional exact-case property keys and case-sensitive value patterns.
    #[serde(default)]
    pub properties: Option<BTreeMap<String, String>>,
}

impl ZeroconfMatcher {
    /// Tests a service using Home Assistant's name and property matching rules.
    pub fn matches(&self, info: &ZeroconfServiceInfo) -> bool {
        if let Some(pattern) = &self.name
            && !python_fnmatchcase(&info.name.to_lowercase(), pattern)
        {
            return false;
        }

        if let Some(properties) = &self.properties {
            for (key, pattern) in properties {
                // Property keys are looked up exactly. Missing and valueless TXT
                // values both fail, while an empty string remains matchable.
                let Some(value) = info.properties.get(key) else {
                    return false;
                };
                if !python_fnmatchcase(&value.to_lowercase(), pattern) {
                    return false;
                }
            }
        }

        true
    }
}

/// A matcher registry indexed by exact DNS-SD service type.
#[derive(Clone, Debug, Default)]
pub struct MatcherIndex {
    ordered_service_types: Vec<String>,
    by_service_type: HashMap<String, Vec<ZeroconfMatcher>>,
}

impl MatcherIndex {
    /// Parses the generated service-type map while retaining its source key order.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let buckets: OrderedBuckets = serde_json::from_str(json)?;
        let mut index = Self {
            ordered_service_types: Vec::with_capacity(buckets.0.len()),
            by_service_type: HashMap::with_capacity(buckets.0.len()),
        };

        for (service_type, matchers) in buckets.0 {
            if let Some(existing) = index.by_service_type.get_mut(&service_type) {
                existing.extend(matchers);
            } else {
                index.ordered_service_types.push(service_type.clone());
                index.by_service_type.insert(service_type, matchers);
            }
        }

        Ok(index)
    }

    /// Loads the pinned Home Assistant generated matcher metadata.
    pub fn builtin() -> Self {
        Self::from_json(include_str!("matchers.json"))
            .expect("bundled Zeroconf matcher metadata must be valid JSON")
    }

    /// Iterates service types in the generated source order.
    pub fn service_types(&self) -> impl Iterator<Item = &str> {
        self.ordered_service_types.iter().map(String::as_str)
    }

    /// Returns all matchers for an exact service type in generated list order.
    pub fn matchers_for(&self, service_type: &str) -> &[ZeroconfMatcher] {
        self.by_service_type
            .get(service_type)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Returns each matching matcher in source list order.
    pub fn matching<'a>(
        &'a self,
        service_type: &str,
        info: &ZeroconfServiceInfo,
    ) -> Vec<&'a ZeroconfMatcher> {
        self.matchers_for(service_type)
            .iter()
            .filter(|matcher| matcher.matches(info))
            .collect()
    }
}

/// Deserializes a JSON object into an ordered sequence of service-type buckets.
struct OrderedBuckets(Vec<(String, Vec<ZeroconfMatcher>)>);

impl<'de> Deserialize<'de> for OrderedBuckets {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OrderedBucketsVisitor;

        impl<'de> Visitor<'de> for OrderedBucketsVisitor {
            type Value = OrderedBuckets;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object of service types to matcher arrays")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut buckets = Vec::with_capacity(map.size_hint().unwrap_or_default());
                while let Some(bucket) = map.next_entry::<String, Vec<ZeroconfMatcher>>()? {
                    buckets.push(bucket);
                }
                Ok(OrderedBuckets(buckets))
            }
        }

        deserializer.deserialize_map(OrderedBucketsVisitor)
    }
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

/// Matches the case-sensitive glob semantics used by Python fnmatchcase.
///
/// Python's translation treats backslashes as ordinary characters, supports
/// *, ?, [seq], [!seq] and ranges, and treats an unmatched [ literally.
/// Matching is performed over Unicode scalar values, with * and ? also
/// matching newlines (the source translator uses DOTALL).
pub(crate) fn python_fnmatchcase(candidate: &str, pattern: &str) -> bool {
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

fn parse_pattern(pattern: &str) -> Vec<PatternToken> {
    let characters: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::with_capacity(characters.len());
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
                    // Python leaves an unmatched opening bracket literal.
                    tokens.push(PatternToken::Literal('['));
                    index += 1;
                }
            }
            literal => {
                // A backslash is a normal character, not an escape character.
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
            // Python translates a negated empty range to a wildcard.
            PatternToken::AnyCharacter
        } else {
            PatternToken::Never
        }
    } else {
        PatternToken::CharacterClass { negated, members }
    };

    Some((token, close_index + 1))
}

#[derive(Clone, Copy)]
struct ClassCharacter {
    value: char,
    escaped: bool,
}

fn parse_class_members(raw_members: &[char], negated: bool) -> Vec<ClassMember> {
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
        // A trailing hyphen is literal, just as in Python's translator.
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
            // Python escapes backslashes and hyphens that are literal within a chunk.
            escaped: value == '\\' || value == '-',
        }));
        if index + 1 < chunks.len() {
            // A hyphen between chunks is a possible range separator.
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
            // Reversed ranges are removed by Python's fnmatch translator.
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
    use super::{MatcherIndex, ZeroconfMatcher, python_fnmatchcase};
    use crate::models::{
        RawResolvedServiceInfo, RawTxtEntry, ScopedIpAddress, ZeroconfServiceInfo,
    };
    use std::net::IpAddr;

    fn info(name: &str, txt: Vec<(&[u8], Option<&[u8]>)>) -> ZeroconfServiceInfo {
        let raw = RawResolvedServiceInfo {
            ip_addresses: vec![ScopedIpAddress::new(
                "192.168.1.5".parse::<IpAddr>().unwrap(),
                0,
            )],
            port: Some(80),
            hostname: Some("host.local.".to_owned()),
            service_type: "_example._tcp.local.".to_owned(),
            name: name.to_owned(),
            txt_entries: txt
                .into_iter()
                .map(|(key, value)| RawTxtEntry {
                    key: key.to_vec(),
                    value: value.map(<[u8]>::to_vec),
                })
                .collect(),
        };
        ZeroconfServiceInfo::from_raw(raw).expect("usable address")
    }

    fn matcher(name: Option<&str>, properties: Option<Vec<(&str, &str)>>) -> ZeroconfMatcher {
        ZeroconfMatcher {
            domain: "example".to_owned(),
            name: name.map(str::to_owned),
            properties: properties.map(|entries| {
                entries
                    .into_iter()
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .collect()
            }),
        }
    }

    #[test]
    fn matcher_preserves_candidate_lowercasing_and_original_patterns() {
        let service = info("Kitchen Lamp._example._tcp.local.", vec![]);
        assert!(matcher(Some("kitchen lamp.*"), None).matches(&service));
        assert!(!matcher(Some("Kitchen Lamp.*"), None).matches(&service));

        let service = info(
            "device._example._tcp.local.",
            vec![(b"model", Some(b"SAMSUNG Frame"))],
        );
        assert!(matcher(None, Some(vec![("model", "samsung*")])).matches(&service));
        assert!(!matcher(None, Some(vec![("model", "SAMSUNG*")])).matches(&service));
        assert!(!matcher(None, Some(vec![("Model", "samsung*")])).matches(&service));
    }

    #[test]
    fn missing_and_valueless_properties_fail_but_empty_values_are_present() {
        let service = info(
            "device._example._tcp.local.",
            vec![(b"flag", None), (b"empty", Some(b""))],
        );
        assert!(!matcher(None, Some(vec![("missing", "*")])).matches(&service));
        assert!(!matcher(None, Some(vec![("flag", "*")])).matches(&service));
        assert!(matcher(None, Some(vec![("empty", "*")])).matches(&service));
    }

    #[test]
    fn domain_only_matcher_matches_every_instance_of_its_type() {
        let service = info("anything._example._tcp.local.", vec![]);
        assert!(matcher(None, None).matches(&service));
    }

    #[test]
    fn fnmatch_matches_python_brackets_ranges_negation_and_backslashes() {
        assert!(python_fnmatchcase("a", "[a-c]"));
        assert!(python_fnmatchcase("c", "[a-c]"));
        assert!(!python_fnmatchcase("d", "[a-c]"));
        assert!(python_fnmatchcase("d", "[!a-c]"));
        assert!(!python_fnmatchcase("b", "[!a-c]"));
        assert!(python_fnmatchcase("d", "[a-b-c-d]"));
        assert!(python_fnmatchcase("x", "[!z-a]"));
        assert!(python_fnmatchcase("c", "[a--c]"));
        assert!(!python_fnmatchcase("a", "[a--c]"));
        assert!(python_fnmatchcase("b", "[z-ab]"));
        assert!(!python_fnmatchcase("c", "[z-ab]"));
        assert!(!python_fnmatchcase("z", "[z-a]"));
        assert!(python_fnmatchcase("[abc", "[abc"));
        assert!(python_fnmatchcase("[]", "[]"));
        assert!(python_fnmatchcase("]", "[]]"));
        assert!(python_fnmatchcase("a", "[!]]"));
        assert!(!python_fnmatchcase("]", "[!]]"));
        assert!(python_fnmatchcase("[!]", "[!]"));
        assert!(python_fnmatchcase("\\device", "\\*"));
        assert!(!python_fnmatchcase("device", "\\*"));
        assert!(python_fnmatchcase("\\", r"[\\]"));
        assert!(python_fnmatchcase("-", "[-a]"));
        assert!(python_fnmatchcase("-", "[a-]"));
        assert!(python_fnmatchcase("\n", "?"));
    }

    #[test]
    fn generated_index_preserves_bucket_and_matcher_order() {
        let index = MatcherIndex::from_json(
            r#"{"_second._tcp.local.":[{"domain":"second"}],"_first._tcp.local.":[{"domain":"first"},{"domain":"first-two","name":"*"}]}"#,
        )
        .expect("valid matcher data");
        assert_eq!(
            index.service_types().collect::<Vec<_>>(),
            ["_second._tcp.local.", "_first._tcp.local."]
        );
        assert_eq!(index.matchers_for("_first._tcp.local.").len(), 2);
        assert!(index.matchers_for("_absent._tcp.local.").is_empty());
    }

    #[test]
    fn bundled_registry_contains_the_pinned_generated_matchers() {
        let index = MatcherIndex::builtin();
        assert_eq!(index.service_types().count(), 120);
        assert_eq!(
            index
                .ordered_service_types
                .iter()
                .map(|service_type| index.matchers_for(service_type).len())
                .sum::<usize>(),
            174
        );
    }
}
