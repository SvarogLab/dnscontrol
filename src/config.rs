use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;

/// Where a document came from, for error messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocRef {
    pub file: String,
    pub index: usize,
}

impl std::fmt::Display for DocRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} document {}", self.file, self.index)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocKind {
    Snippets,
    Zone,
}

/// The union of every document shape.
///
/// Deliberately not `#[serde(tag = "kind")]`: serde's internally-tagged representation buffers
/// every value through `deserialize_any`, which is precisely the type-inference path serde-saphyr
/// exists to avoid. Under it `name: no` comes back as a bool and `target: [12345]` as an integer —
/// both verified, see `tagged_enum_would_lose_the_scalar_schema` below. A flat union struct keeps
/// each field's Rust type as the schema, and `classify` does the discrimination.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDocument {
    pub kind: Option<DocKind>,
    pub zone: Option<String>,
    pub records: Option<Vec<RawEntry>>,
    pub ignore: Option<Vec<RawIgnore>>,
    pub snippets: Option<BTreeMap<String, Vec<RawEntry>>>,
}

/// One entry in a `records:` list or a snippet body: either `use: <name>` or a record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawEntry {
    #[serde(rename = "use")]
    pub use_: Option<String>,
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub rtype: Option<String>,
    pub ttl: Option<u32>,
    pub target: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawIgnore {
    pub name: String,
    #[serde(rename = "type")]
    pub rtype: Option<String>,
}

/// A `RawEntry` after discrimination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Use(String),
    Record(RawRecord),
}

/// A record with its required fields proven present, but not yet normalized: the type is still
/// lowercase, the name still a relative label, the TTL still optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRecord {
    pub name: Option<String>,
    pub rtype: String,
    pub ttl: Option<u32>,
    pub target: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Document {
    Snippets(BTreeMap<String, Vec<Entry>>),
    Zone {
        zone: String,
        records: Vec<Entry>,
        ignore: Vec<RawIgnore>,
    },
}

/// YAML anchors and merge keys are rejected: the extension mechanism here is `use:`, and accepting
/// `<<:` as well would give two ways to do one thing.
fn options() -> serde_saphyr::Options {
    let mut o = serde_saphyr::Options::default();
    o.merge_keys = serde_saphyr::MergeKeyPolicy::Error;
    o
}

pub fn parse_documents(file: &str, content: &str) -> Result<Vec<(DocRef, Document)>> {
    let raw: Vec<RawDocument> = serde_saphyr::from_multiple_with_options(content, options())
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("failed to parse {file}"))?;

    raw.into_iter()
        .enumerate()
        .map(|(index, doc)| {
            let at = DocRef {
                file: file.to_string(),
                index,
            };
            let classified = classify(&doc, &at)?;
            Ok((at, classified))
        })
        .collect()
}

pub fn classify(doc: &RawDocument, at: &DocRef) -> Result<Document> {
    let Some(kind) = doc.kind else {
        bail!("{at} is missing the required \"kind:\" key (expected \"zone\" or \"snippets\")");
    };

    match kind {
        DocKind::Zone => {
            if doc.snippets.is_some() {
                bail!("{at} is a zone document but also declares \"snippets:\"");
            }
            let Some(zone) = doc.zone.as_deref() else {
                bail!("{at} is a zone document but is missing \"zone:\"");
            };
            if zone.trim().is_empty() {
                bail!("{at} has an empty \"zone:\"");
            }
            let records = doc
                .records
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|e| classify_entry(e, at))
                .collect::<Result<Vec<_>>>()?;
            Ok(Document::Zone {
                zone: zone.to_string(),
                records,
                ignore: doc.ignore.clone().unwrap_or_default(),
            })
        }
        DocKind::Snippets => {
            for (field, present) in [
                ("zone", doc.zone.is_some()),
                ("records", doc.records.is_some()),
                ("ignore", doc.ignore.is_some()),
            ] {
                if present {
                    bail!("{at} is a snippets document but also declares \"{field}:\"");
                }
            }
            let Some(snippets) = doc.snippets.as_ref() else {
                bail!("{at} is a snippets document but is missing \"snippets:\"");
            };
            let mut out = BTreeMap::new();
            for (name, entries) in snippets {
                if name.trim().is_empty() {
                    bail!("{at} declares a snippet with an empty name");
                }
                let entries = entries
                    .iter()
                    .map(|e| classify_entry(e, at))
                    .collect::<Result<Vec<_>>>()?;
                out.insert(name.clone(), entries);
            }
            Ok(Document::Snippets(out))
        }
    }
}

pub fn classify_entry(entry: &RawEntry, at: &DocRef) -> Result<Entry> {
    let record_fields = [
        ("name", entry.name.is_some()),
        ("type", entry.rtype.is_some()),
        ("ttl", entry.ttl.is_some()),
        ("target", entry.target.is_some()),
    ];

    if let Some(name) = &entry.use_ {
        if let Some((field, _)) = record_fields.iter().find(|(_, present)| *present) {
            bail!("{at}: entry \"use: {name}\" cannot also set \"{field}:\"");
        }
        if name.trim().is_empty() {
            bail!("{at}: entry has an empty \"use:\"");
        }
        return Ok(Entry::Use(name.clone()));
    }

    if record_fields.iter().all(|(_, present)| !present) {
        bail!("{at}: entry is empty — it must be either \"use: <snippet>\" or a record");
    }
    let Some(rtype) = entry.rtype.as_deref() else {
        bail!("{at}: record entry is missing \"type:\"");
    };
    let Some(target) = entry.target.as_deref() else {
        bail!("{at}: record entry {rtype} is missing \"target:\"");
    };

    Ok(Entry::Record(RawRecord {
        name: entry.name.clone(),
        rtype: rtype.to_string(),
        ttl: entry.ttl,
        target: target.to_vec(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> Result<Vec<(DocRef, Document)>> {
        parse_documents("test.yaml", content)
    }

    fn err(content: &str) -> String {
        format!("{:#}", parse(content).expect_err("should have failed"))
    }

    fn records(doc: &Document) -> &[Entry] {
        match doc {
            Document::Zone { records, .. } => records,
            other => panic!("expected a zone document, got {other:?}"),
        }
    }

    fn first_record(content: &str) -> RawRecord {
        let docs = parse(content).expect("parses");
        match &records(&docs[0].1)[0] {
            Entry::Record(r) => r.clone(),
            other => panic!("expected a record, got {other:?}"),
        }
    }

    // ---- serde-saphyr scalar schema -------------------------------------------------------
    // These are the contract this whole config format rests on. If any of them regresses, the
    // union-struct design in RawDocument is no longer buying anything.

    #[test]
    fn unquoted_ipv4_target_stays_a_string() {
        let r = first_record(
            "kind: zone\nzone: example.com\nrecords:\n  - {type: a, target: [192.0.2.10]}\n",
        );
        assert_eq!(r.target, vec!["192.0.2.10".to_string()]);
    }

    #[test]
    fn unquoted_mx_target_with_priority_stays_a_string() {
        let r = first_record(
            "kind: zone\nzone: example.com\nrecords:\n  - {type: mx, target: [\"10 mx1.mail.example.net.\"]}\n",
        );
        assert_eq!(r.target, vec!["10 mx1.mail.example.net.".to_string()]);
    }

    #[test]
    fn numeric_looking_txt_target_stays_a_string() {
        let r = first_record(
            "kind: zone\nzone: example.com\nrecords:\n  - {type: txt, target: [12345]}\n",
        );
        assert_eq!(r.target, vec!["12345".to_string()]);
    }

    /// The "Norway problem": `no` is a YAML 1.1 boolean and a perfectly good DNS label.
    #[test]
    fn norway_label_stays_a_string() {
        let r = first_record(
            "kind: zone\nzone: example.com\nrecords:\n  - {name: no, type: a, target: [x]}\n",
        );
        assert_eq!(r.name.as_deref(), Some("no"));
    }

    #[test]
    fn ttl_parses_as_an_integer() {
        let r = first_record(
            "kind: zone\nzone: example.com\nrecords:\n  - {type: a, ttl: 300, target: [x]}\n",
        );
        assert_eq!(r.ttl, Some(300));
    }

    /// Documents why `RawDocument` is a flat union struct rather than a tagged enum. If this ever
    /// starts passing, serde-saphyr has fixed internally-tagged enums and `classify` could go.
    #[test]
    fn tagged_enum_would_lose_the_scalar_schema() {
        #[derive(Debug, Deserialize)]
        #[serde(tag = "kind", rename_all = "lowercase")]
        #[allow(dead_code)]
        enum Tagged {
            Zone { records: Vec<RawEntry> },
        }

        let norway: Result<Tagged, _> =
            serde_saphyr::from_str("kind: zone\nrecords:\n  - {name: no, type: a, target: [x]}\n");
        assert!(norway.is_err(), "`name: no` should degrade to a bool");

        let numeric: Result<Tagged, _> =
            serde_saphyr::from_str("kind: zone\nrecords:\n  - {type: txt, target: [12345]}\n");
        assert!(
            numeric.is_err(),
            "`target: [12345]` should degrade to an int"
        );
    }

    // ---- document structure ---------------------------------------------------------------

    #[test]
    fn parses_a_snippets_and_zone_stream() {
        let docs = parse(
            "kind: snippets\n\
             snippets:\n  \
               mail-mx:\n    - {type: mx, ttl: 300, target: [\"10 mx1.mail.example.net.\"]}\n  \
               mail-stack:\n    - {use: mail-mx}\n\
             ---\n\
             kind: zone\n\
             zone: example.com\n\
             records:\n  \
               - {use: mail-stack}\n  \
               - {name: www, type: a, target: [192.0.2.10]}\n",
        )
        .expect("parses");

        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].0.to_string(), "test.yaml document 0");
        let Document::Snippets(snippets) = &docs[0].1 else {
            panic!("expected snippets, got {:?}", docs[0].1)
        };
        assert_eq!(snippets.len(), 2);
        assert_eq!(snippets["mail-stack"], vec![Entry::Use("mail-mx".into())]);

        let Document::Zone { zone, records, .. } = &docs[1].1 else {
            panic!("expected a zone, got {:?}", docs[1].1)
        };
        assert_eq!(zone, "example.com");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], Entry::Use("mail-stack".into()));
    }

    #[test]
    fn zone_may_declare_ignore_rules() {
        let docs = parse(
            "kind: zone\nzone: example.com\nignore:\n  - {name: \"_dnsauth*\", type: txt}\n  - {name: legacy}\nrecords: []\n",
        )
        .expect("parses");
        let Document::Zone { ignore, .. } = &docs[0].1 else {
            panic!("expected a zone")
        };
        assert_eq!(ignore.len(), 2);
        assert_eq!(ignore[0].name, "_dnsauth*");
        assert_eq!(ignore[0].rtype.as_deref(), Some("txt"));
        assert_eq!(ignore[1].rtype, None);
    }

    #[test]
    fn a_zone_without_records_is_allowed() {
        let docs = parse("kind: zone\nzone: example.com\n").expect("parses");
        assert!(records(&docs[0].1).is_empty());
    }

    #[test]
    fn empty_content_yields_no_documents() {
        assert!(parse("").expect("parses").is_empty());
    }

    #[test]
    fn mixed_kinds_in_one_file_are_allowed() {
        let docs = parse(
            "kind: zone\nzone: example.com\n---\nkind: snippets\nsnippets: {}\n---\nkind: zone\nzone: example.net\n",
        )
        .expect("parses");
        assert_eq!(docs.len(), 3);
        assert_eq!(docs[2].0.index, 2);
    }

    // ---- rejections ------------------------------------------------------------------------

    #[test]
    fn missing_kind_is_an_error() {
        assert!(err("zone: example.com\n").contains("missing the required \"kind:\""));
    }

    #[test]
    fn unknown_kind_is_an_error() {
        assert!(!err("kind: bogus\n").is_empty());
    }

    #[test]
    fn zone_document_without_zone_key_is_an_error() {
        assert!(err("kind: zone\nrecords: []\n").contains("missing \"zone:\""));
    }

    #[test]
    fn zone_document_with_snippets_key_is_an_error() {
        assert!(
            err("kind: zone\nzone: example.com\nsnippets: {}\n")
                .contains("also declares \"snippets:\"")
        );
    }

    #[test]
    fn snippets_document_with_zone_key_is_an_error() {
        assert!(
            err("kind: snippets\nsnippets: {}\nzone: example.com\n")
                .contains("also declares \"zone:\"")
        );
    }

    #[test]
    fn snippets_document_without_snippets_key_is_an_error() {
        assert!(err("kind: snippets\n").contains("missing \"snippets:\""));
    }

    #[test]
    fn unknown_field_is_an_error() {
        assert!(err("kind: zone\nzone: example.com\nbogus: 1\n").contains("failed to parse"));
    }

    #[test]
    fn merge_key_is_rejected() {
        let e = err("kind: zone\nzone: example.com\ndefaults: &d {a: 1}\n<<: *d\n");
        assert!(e.contains("failed to parse"), "unexpected error: {e}");
    }

    #[test]
    fn entry_with_both_use_and_type_is_an_error() {
        assert!(
            err("kind: zone\nzone: example.com\nrecords:\n  - {use: mx, type: a}\n")
                .contains("cannot also set \"type:\"")
        );
    }

    #[test]
    fn empty_entry_is_an_error() {
        assert!(
            err("kind: zone\nzone: example.com\nrecords:\n  - {}\n").contains("entry is empty")
        );
    }

    #[test]
    fn record_without_type_is_an_error() {
        assert!(
            err("kind: zone\nzone: example.com\nrecords:\n  - {name: www, target: [x]}\n")
                .contains("missing \"type:\"")
        );
    }

    #[test]
    fn record_without_target_is_an_error() {
        assert!(
            err("kind: zone\nzone: example.com\nrecords:\n  - {name: www, type: a}\n")
                .contains("missing \"target:\"")
        );
    }

    #[test]
    fn malformed_yaml_is_an_error() {
        assert!(err("kind: zone\n  bad indent: [\n").contains("failed to parse"));
    }

    #[test]
    fn errors_name_the_offending_document() {
        let e = err("kind: zone\nzone: example.com\n---\nzone: example.net\n");
        assert!(e.contains("test.yaml document 1"), "unexpected error: {e}");
    }
}
