//! Pure light-profile parsing and merge behavior.

use crate::color;
use csv::{ReaderBuilder, Trim};
use std::collections::HashMap;

/// Built-in profiles from Home Assistant's pinned `light_profiles.csv`.
pub const BUILT_IN_LIGHT_PROFILES_CSV: &str = "id,x,y,brightness\nrelax,0.5119,0.4147,144\nconcentrate,0.5119,0.4147,219\nenergize,0.368,0.3686,203\nreading,0.4448,0.4066,240\n";

/// Parsed Home Assistant light profile.
#[derive(Clone, Debug, PartialEq)]
pub struct LightProfile {
    /// Profile key as written in CSV. Whitespace and case are preserved.
    pub name: String,
    /// Optional CIE x coordinate.
    pub color_x: Option<f64>,
    /// Optional CIE y coordinate.
    pub color_y: Option<f64>,
    /// Optional brightness byte.
    pub brightness: Option<u8>,
    /// Optional transition duration, clamped to Core's supported range.
    pub transition: Option<f64>,
}

impl LightProfile {
    /// Derive HS from the stored xy coordinates when both are present.
    #[must_use]
    pub fn hs_color(&self) -> Option<[f64; 2]> {
        Some(color::rgb_to_hs(color::xy_to_rgb([
            self.color_x?,
            self.color_y?,
        ])))
    }
}

/// Ordered profile catalog merged from built-in and custom CSV strings.
///
/// This type has no filesystem or global state. Hosts read their available
/// sources and pass the text to [`LightProfileCatalog::from_csv_sources`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LightProfileCatalog {
    profiles: HashMap<String, LightProfile>,
}

impl LightProfileCatalog {
    /// Parse the four built-in profiles shipped with the pinned Home Assistant
    /// revision.
    #[must_use]
    pub fn built_in() -> Self {
        Self::from_csv_sources([BUILT_IN_LIGHT_PROFILES_CSV])
    }

    /// Parse each CSV source in order, with later profile names replacing
    /// earlier entries. Every source skips exactly one header record. A blank
    /// record or malformed profile stops that source while later sources are
    /// still processed. Use [`LightProfileCatalog::from_csv_sources_with_report`]
    /// when the host needs row diagnostics.
    #[must_use]
    pub fn from_csv_sources<S>(sources: impl IntoIterator<Item = S>) -> Self
    where
        S: AsRef<str>,
    {
        Self::from_csv_sources_with_report(sources).0
    }

    /// Parse and merge CSV sources, returning the catalog and one diagnostic
    /// for the first invalid row in each source. `source_index` identifies the
    /// input's position so a host can attach its own file path without the SDK
    /// performing filesystem IO.
    #[must_use]
    pub fn from_csv_sources_with_report<S>(
        sources: impl IntoIterator<Item = S>,
    ) -> (Self, Vec<LightProfileParseIssue>)
    where
        S: AsRef<str>,
    {
        let mut catalog = Self::default();
        let mut issues = Vec::new();
        for (source_index, source) in sources.into_iter().enumerate() {
            catalog.merge_csv_source(source_index, source.as_ref(), &mut issues);
        }
        (catalog, issues)
    }

    /// Look up a profile by its exact, untrimmed name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&LightProfile> {
        self.profiles.get(name)
    }

    /// Return the number of valid profiles in the catalog.
    #[must_use]
    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    /// Return whether the catalog has no profiles.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    fn merge_csv_source(
        &mut self,
        source_index: usize,
        source: &str,
        issues: &mut Vec<LightProfileParseIssue>,
    ) {
        let mut records = logical_records(source).into_iter();
        // Core calls next(reader, None) once, even when this is an empty or
        // blank-header file.
        let _ = records.next();

        for (record_index, record) in records.enumerate() {
            let Some(record) = record else {
                // Python's csv.reader yields [] for an empty physical line;
                // that is an invalid profile and aborts the rest of this file.
                issues.push(LightProfileParseIssue {
                    source_index,
                    record_index: record_index + 1,
                    row: Some(Vec::new()),
                    raw_record: String::new(),
                    reason: "blank profile row",
                });
                break;
            };
            let Some(fields) = parse_csv_record(record) else {
                issues.push(LightProfileParseIssue {
                    source_index,
                    record_index: record_index + 1,
                    row: None,
                    raw_record: record.to_owned(),
                    reason: "invalid CSV record",
                });
                break;
            };
            let profile = match LightProfile::from_fields(&fields) {
                Ok(profile) => profile,
                Err(reason) => {
                    issues.push(LightProfileParseIssue {
                        source_index,
                        record_index: record_index + 1,
                        row: Some(fields),
                        raw_record: record.to_owned(),
                        reason,
                    });
                    break;
                }
            };
            self.profiles.insert(profile.name.clone(), profile);
        }
    }
}

impl LightProfile {
    fn from_fields(fields: &[String]) -> Result<Self, &'static str> {
        if fields.len() != 4 && fields.len() != 5 {
            return Err("expected exactly four or five fields");
        }
        let color_x = optional_float(&fields[1]).ok_or("invalid x coordinate")?;
        let color_y = optional_float(&fields[2]).ok_or("invalid y coordinate")?;
        let brightness = optional_byte(&fields[3]).ok_or("invalid brightness byte")?;
        let transition = fields
            .get(4)
            .map(|value| optional_float(value).ok_or("invalid transition"))
            .transpose()?
            .flatten()
            .map(|value| value.clamp(0.0, 6_553.0));

        Ok(Self {
            name: fields[0].clone(),
            color_x,
            color_y,
            brightness,
            transition,
        })
    }
}

/// Parse issue returned with a profile catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightProfileParseIssue {
    /// Zero-based index of the CSV string supplied to the parser.
    pub source_index: usize,
    /// One-based logical record index after the skipped header.
    pub record_index: usize,
    /// Parsed fields when CSV parsing succeeded; a blank record is an empty
    /// vector. `None` means CSV syntax prevented field extraction.
    pub row: Option<Vec<String>>,
    /// Raw logical record text, including its original line ending when any.
    pub raw_record: String,
    /// Stable short explanation for the row rejection.
    pub reason: &'static str,
}

fn optional_float(value: &str) -> Option<Option<f64>> {
    if value.is_empty() {
        return Some(None);
    }
    let parsed = value.trim().parse::<f64>().ok()?;
    if !parsed.is_finite() {
        return None;
    }
    Some(Some(parsed))
}

fn optional_byte(value: &str) -> Option<Option<u8>> {
    if value.is_empty() {
        return Some(None);
    }
    let parsed = value.trim().parse::<i64>().ok()?;
    Some(Some(u8::try_from(parsed).ok()?))
}

/// Split CSV into logical records while retaining empty physical records.
///
/// Rust's CSV reader skips blank lines. Home Assistant's Python CSV reader
/// returns an empty row for one, so it must be surfaced to the catalog merger
/// to stop that file. Quoted CR/LF characters remain part of the logical
/// record, including escaped double quotes.
fn logical_records(source: &str) -> Vec<Option<&str>> {
    let bytes = source.as_bytes();
    let mut records = Vec::new();
    let mut record_start = 0;
    let mut in_quotes = false;
    let mut field_start = true;
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'"' if in_quotes && bytes.get(index + 1) == Some(&b'"') => {
                index += 2;
            }
            b'"' if in_quotes => {
                in_quotes = false;
                field_start = false;
                index += 1;
            }
            b'"' if field_start => {
                in_quotes = true;
                field_start = false;
                index += 1;
            }
            b'"' => {
                field_start = false;
                index += 1;
            }
            b',' if !in_quotes => {
                field_start = true;
                index += 1;
            }
            b'\r' | b'\n' => {
                let delimiter_length =
                    if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
                        2
                    } else {
                        1
                    };
                if !in_quotes {
                    let record_end = index;
                    let is_empty = record_end == record_start;
                    records.push(if is_empty {
                        None
                    } else {
                        Some(&source[record_start..index + delimiter_length])
                    });
                    record_start = index + delimiter_length;
                    field_start = true;
                }
                index += delimiter_length;
            }
            _ => {
                if !in_quotes {
                    field_start = false;
                }
                index += 1;
            }
        }
    }

    if record_start < bytes.len() {
        records.push(Some(&source[record_start..]));
    }
    records
}

fn parse_csv_record(source: &str) -> Option<Vec<String>> {
    // Python opens profile files in text mode with universal newlines. Normalize
    // quoted multiline field content the same way before passing it to Rust CSV.
    let source = source.replace("\r\n", "\n").replace('\r', "\n");
    let mut reader = ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .trim(Trim::None)
        .from_reader(source.as_bytes());
    let mut records = reader.records();
    let first = records.next()?.ok()?;
    // Framing should hand us exactly one logical record. Treat any mismatch as
    // malformed input rather than accidentally merging unrelated rows.
    if records.next().is_some() {
        return None;
    }
    Some(first.iter().map(str::to_owned).collect())
}

#[cfg(test)]
mod tests {
    use super::{BUILT_IN_LIGHT_PROFILES_CSV, LightProfileCatalog};

    #[test]
    fn builtins_match_pinned_values() {
        let profiles = LightProfileCatalog::built_in();
        assert_eq!(profiles.len(), 4);
        for (name, xy, brightness) in [
            ("relax", [0.5119, 0.4147], 144),
            ("concentrate", [0.5119, 0.4147], 219),
            ("energize", [0.368, 0.3686], 203),
            ("reading", [0.4448, 0.4066], 240),
        ] {
            let profile = profiles.get(name).unwrap();
            assert_eq!(profile.color_x, Some(xy[0]));
            assert_eq!(profile.color_y, Some(xy[1]));
            assert_eq!(profile.brightness, Some(brightness));
            assert_eq!(profile.transition, None);
            assert!(profile.hs_color().is_some());
        }
        assert_eq!(BUILT_IN_LIGHT_PROFILES_CSV.lines().count(), 5);
    }

    #[test]
    fn custom_profiles_override_builtin_names_and_support_four_or_five_columns() {
        let custom = concat!(
            "name,x,y,brightness,transition\n",
            "relax,,,,12.5\n",
            "custom, 0.1 ,0.2,255,9000\n",
            "partial,,0.2,,\n"
        );
        let profiles = LightProfileCatalog::from_csv_sources([BUILT_IN_LIGHT_PROFILES_CSV, custom]);
        let relax = profiles.get("relax").unwrap();
        assert_eq!(relax.color_x, None);
        assert_eq!(relax.color_y, None);
        assert_eq!(relax.brightness, None);
        assert_eq!(relax.transition, Some(12.5));
        let custom = profiles.get("custom").unwrap();
        assert_eq!(custom.color_x, Some(0.1));
        assert_eq!(custom.color_y, Some(0.2));
        assert_eq!(custom.brightness, Some(255));
        assert_eq!(custom.transition, Some(6_553.0));
        let partial = profiles.get("partial").unwrap();
        assert_eq!(partial.hs_color(), None);
    }

    #[test]
    fn quoted_fields_and_multiline_values_are_preserved() {
        let profiles = LightProfileCatalog::from_csv_sources([concat!(
            "name,x,y,brightness,transition\r\n",
            "\"two\nlines\",0.3,0.4,7,2\r\n",
            "\"crlf\r\nlines\",0.1,0.2,6,1\r\n",
            "\"quote \"\"inside\"\"\",0.2,0.3,8,3\r\n"
        )]);
        assert_eq!(profiles.get("two\nlines").unwrap().brightness, Some(7));
        assert_eq!(profiles.get("crlf\nlines").unwrap().brightness, Some(6));
        assert_eq!(
            profiles.get("quote \"inside\"").unwrap().brightness,
            Some(8)
        );
    }

    #[test]
    fn skips_one_header_and_stops_each_file_at_blank_or_invalid_rows() {
        let first = concat!("\n", "before,,,,1\n", "\n", "after,,,,2\n");
        let second = concat!(
            "header,x,y,brightness,transition\n",
            "kept,,,,3\n",
            "bad,,,,not-a-number\n",
            "ignored,,,,4\n"
        );
        let profiles = LightProfileCatalog::from_csv_sources([first, second]);
        assert_eq!(profiles.get("before").unwrap().transition, Some(1.0));
        assert_eq!(profiles.get("after"), None);
        assert_eq!(profiles.get("kept").unwrap().transition, Some(3.0));
        assert_eq!(profiles.get("ignored"), None);
    }

    #[test]
    fn exact_empty_is_optional_but_whitespace_only_numbers_abort_the_file() {
        let profiles = LightProfileCatalog::from_csv_sources([concat!(
            "header,x,y,brightness,transition\n",
            "empty,,,,\n",
            "spaced,, , , \n",
            "after,,,,2\n"
        )]);
        let empty = profiles.get("empty").unwrap();
        assert_eq!(empty.color_x, None);
        assert_eq!(empty.color_y, None);
        assert_eq!(empty.brightness, None);
        assert_eq!(empty.transition, None);
        assert_eq!(profiles.get("spaced"), None);
        assert_eq!(profiles.get("after"), None);
    }

    #[test]
    fn names_are_not_trimmed_and_later_source_wins() {
        let profiles = LightProfileCatalog::from_csv_sources([
            "header,x,y,brightness\nname,,,1\n",
            "header,x,y,brightness\n name ,,,2\nname,,,3\n",
        ]);
        assert_eq!(profiles.get("name").unwrap().brightness, Some(3));
        assert_eq!(profiles.get(" name ").unwrap().brightness, Some(2));
    }

    #[test]
    fn report_identifies_first_bad_record_by_source_and_preserves_row_text() {
        let (profiles, issues) = LightProfileCatalog::from_csv_sources_with_report([
            "header,x,y,brightness,transition\nkept,,,,1\nbad,,,,nope\nignored,,,,2\n",
            "header,x,y,brightness\ncustom,,,3\n",
        ]);
        assert_eq!(profiles.get("kept").unwrap().transition, Some(1.0));
        assert_eq!(profiles.get("ignored"), None);
        assert_eq!(profiles.get("custom").unwrap().brightness, Some(3));
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].source_index, 0);
        assert_eq!(issues[0].record_index, 2);
        assert_eq!(
            issues[0].row.as_ref().unwrap(),
            &["bad", "", "", "", "nope"].map(str::to_owned)
        );
        assert_eq!(issues[0].reason, "invalid transition");
        assert_eq!(issues[0].raw_record, "bad,,,,nope\n");
    }
}
