use crate::config::{DocRef, Document, RawIgnore, RawRecord};
use crate::model::{Desired, DesiredZone, IgnoreRule, Rrset, default_ignore_rules, label_of};
use crate::snippets::{Registry, collect_snippets, expand};
use anyhow::{Result, bail};
use std::collections::BTreeMap;

/// TTL applied to a record that declares none.
pub const DEFAULT_TTL: u32 = 900;

/// Cloud DNS owns these at the apex; this tool never writes or deletes them.
const APEX_RESERVED: [&str; 2] = ["SOA", "NS"];

/// `Example.Com.` → (`example.com.`, `example-com`). Case and a trailing dot are forgiven; there
/// is nothing ambiguous about either.
pub fn zone_names(zone: &str) -> Result<(String, String)> {
    let base = zone.trim().trim_end_matches('.').to_ascii_lowercase();
    if base.is_empty() {
        bail!("zone name is empty");
    }
    let resource_name = base.replace('.', "-");
    if !is_valid_resource_name(&resource_name) {
        bail!(
            "zone \"{zone}\" produces the Cloud DNS resource name \"{resource_name}\", which is \
             not valid (must start with a letter, then lowercase letters, digits or dashes, at \
             most 63 characters)"
        );
    }
    Ok((format!("{base}."), resource_name))
}

fn is_valid_resource_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && name.len() <= 63
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn normalize_record(record: &RawRecord, dns_name: &str, default_ttl: u32) -> Result<Rrset> {
    let zone_base = dns_name.trim_end_matches('.');

    let name = match record.name.as_deref() {
        None => dns_name.to_string(),
        Some(label) => {
            // DNS is case-insensitive and Cloud DNS stores names lowercased. Folding here is what
            // makes the FQDN guard below actually fire on `WWW.EXAMPLE.COM`, and what stops `WWW`
            // and `www` from counting as two different record sets.
            let label = label.trim().to_ascii_lowercase();
            let label = label.as_str();
            if label.is_empty() || label == "@" {
                bail!(
                    "record name must be a relative label; omit \"name\" entirely for the apex of \
                     zone {zone_base}"
                );
            }
            if label.ends_with('.')
                || label == zone_base
                || label.ends_with(&format!(".{zone_base}"))
            {
                let suggestion = label
                    .trim_end_matches('.')
                    .trim_end_matches(zone_base)
                    .trim_end_matches('.');
                bail!(
                    "record name \"{label}\" in zone {zone_base} looks like an FQDN; names are \
                     relative labels — write \"{suggestion}\""
                );
            }
            if label.starts_with('.')
                || label.contains("..")
                || label.split_whitespace().count() != 1
            {
                bail!("record name \"{label}\" in zone {zone_base} is not a valid label");
            }
            format!("{label}.{dns_name}")
        }
    };

    let rtype = record.rtype.trim().to_ascii_uppercase();
    if rtype.is_empty()
        || !rtype
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        bail!(
            "record type \"{}\" in zone {zone_base} is not a DNS record type",
            record.rtype
        );
    }
    if name == dns_name && APEX_RESERVED.contains(&rtype.as_str()) {
        bail!(
            "zone {zone_base} declares the apex {rtype} record set; Cloud DNS owns those and this \
             tool never touches them — remove it"
        );
    }

    if record.target.is_empty() {
        bail!(
            "record {}/{rtype} in zone {zone_base} has no \"target\"",
            short(&name, dns_name)
        );
    }
    if let Some(bad) = record.target.iter().find(|t| t.trim().is_empty()) {
        let _ = bad;
        bail!(
            "record {}/{rtype} in zone {zone_base} has an empty \"target\" entry",
            short(&name, dns_name)
        );
    }

    // RFC 2181 makes a TTL a 32-bit *signed* quantity, and Cloud DNS takes an i32. Reject an
    // out-of-range value rather than clamping it: a clamped TTL can never equal the declared one,
    // so the record set would appear to need updating on every single run.
    let ttl = record.ttl.unwrap_or(default_ttl);
    if ttl > i32::MAX as u32 {
        bail!(
            "record {}/{rtype} in zone {zone_base} has ttl {ttl}, above the maximum of {}",
            short(&name, dns_name),
            i32::MAX
        );
    }

    Ok(Rrset {
        name,
        rtype,
        ttl,
        rrdatas: record.target.clone(),
    })
}

/// The label form of an FQDN, for error messages: `www` rather than `www.example.com.`.
fn short<'a>(fqdn: &'a str, dns_name: &str) -> &'a str {
    match label_of(fqdn, dns_name) {
        Some("") => "@",
        Some(label) => label,
        None => fqdn,
    }
}

pub fn normalize_zone(
    zone: &str,
    records: &[RawRecord],
    raw_ignore: &[RawIgnore],
    default_ttl: u32,
) -> Result<DesiredZone> {
    let (dns_name, resource_name) = zone_names(zone)?;
    let zone_base = dns_name.trim_end_matches('.');

    let mut ignore: Vec<IgnoreRule> = raw_ignore
        .iter()
        .map(|r| IgnoreRule {
            name: r.name.clone(),
            rtype: r.rtype.as_ref().map(|t| t.to_ascii_uppercase()),
        })
        .collect();
    ignore.extend(default_ignore_rules());

    let mut rrsets: BTreeMap<_, Rrset> = BTreeMap::new();
    for record in records {
        let rrset = normalize_record(record, &dns_name, default_ttl)?;

        // A record that is both managed and ignored has no coherent meaning: whichever way it
        // resolved would silently surprise someone. Fail instead of picking.
        let label = label_of(&rrset.name, &dns_name).unwrap_or(&rrset.name);
        if let Some(rule) = ignore.iter().find(|r| r.matches(label, &rrset.rtype)) {
            bail!(
                "record \"{}\"/{} in zone {zone_base} matches ignore rule \"{}\" — it cannot be \
                 both managed and ignored",
                rrset.name,
                rrset.rtype,
                rule.name
            );
        }

        if rrsets.insert(rrset.key(), rrset.clone()).is_some() {
            bail!(
                "duplicate record set \"{}/{}\" in zone {zone_base} — a snippet was probably \
                 spliced in twice",
                rrset.name,
                rrset.rtype
            );
        }
    }

    Ok(DesiredZone {
        description: format!("{zone_base} zone"),
        dns_name,
        resource_name,
        rrsets,
        ignore,
    })
}

/// The whole pure pipeline: parsed documents in, desired state out.
pub fn build_desired(docs: &[(DocRef, Document)], default_ttl: u32) -> Result<Desired> {
    let registry: Registry = collect_snippets(docs)?;
    let mut desired = Desired::default();
    let mut sources: BTreeMap<String, DocRef> = BTreeMap::new();

    for (at, doc) in docs {
        let Document::Zone {
            zone,
            records,
            ignore,
        } = doc
        else {
            continue;
        };

        let (dns_name, _) = zone_names(zone)?;
        if let Some(first) = sources.get(&dns_name) {
            bail!(
                "zone {} is declared twice: {first} and {at}",
                dns_name.trim_end_matches('.')
            );
        }

        let expanded = expand(records, &registry, &format!("zone {zone}"))?;
        let normalized = normalize_zone(zone, &expanded, ignore, default_ttl)?;
        sources.insert(dns_name.clone(), at.clone());
        desired.zones.insert(dns_name, normalized);
    }

    Ok(desired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_documents;

    fn raw(name: Option<&str>, rtype: &str, ttl: Option<u32>, target: &[&str]) -> RawRecord {
        RawRecord {
            name: name.map(String::from),
            rtype: rtype.to_string(),
            ttl,
            target: target.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn norm(record: &RawRecord) -> Result<Rrset> {
        normalize_record(record, "example.com.", DEFAULT_TTL)
    }

    fn norm_err(record: &RawRecord) -> String {
        format!("{:#}", norm(record).expect_err("should have failed"))
    }

    fn zone_err(records: &[RawRecord]) -> String {
        format!(
            "{:#}",
            normalize_zone("example.com", records, &[], DEFAULT_TTL)
                .expect_err("should have failed")
        )
    }

    // ---- zone names -------------------------------------------------------------------------

    #[test]
    fn zone_name_maps_dots_to_dashes() {
        assert_eq!(
            zone_names("example.com").unwrap(),
            ("example.com.".to_string(), "example-com".to_string())
        );
    }

    #[test]
    fn zone_name_trailing_dot_and_case_are_tolerated() {
        assert_eq!(
            zone_names("Example.Com.").unwrap(),
            ("example.com.".to_string(), "example-com".to_string())
        );
    }

    #[test]
    fn zone_name_starting_with_a_digit_is_an_error() {
        let e = format!("{:#}", zone_names("1example.com").expect_err("should fail"));
        assert!(e.contains("not valid"), "unexpected: {e}");
        assert!(e.contains("1example-com"), "unexpected: {e}");
    }

    #[test]
    fn empty_zone_name_is_an_error() {
        assert!(zone_names("  ").is_err());
    }

    // ---- record names -----------------------------------------------------------------------

    #[test]
    fn apex_record_gets_the_bare_zone_fqdn() {
        assert_eq!(
            norm(&raw(None, "mx", None, &["10 a."])).unwrap().name,
            "example.com."
        );
    }

    #[test]
    fn labelled_record_gets_label_dot_zone_fqdn() {
        assert_eq!(
            norm(&raw(Some("www"), "a", None, &["192.0.2.10"]))
                .unwrap()
                .name,
            "www.example.com."
        );
    }

    #[test]
    fn multi_label_name_is_kept() {
        assert_eq!(
            norm(&raw(Some("a.b"), "a", None, &["192.0.2.10"]))
                .unwrap()
                .name,
            "a.b.example.com."
        );
    }

    #[test]
    fn wildcard_label_is_literal() {
        assert_eq!(
            norm(&raw(Some("*"), "txt", None, &["x"])).unwrap().name,
            "*.example.com."
        );
    }

    #[test]
    fn fqdn_style_record_name_is_an_error() {
        let e = norm_err(&raw(Some("www.example.com."), "a", None, &["192.0.2.10"]));
        assert!(e.contains("looks like an FQDN"), "unexpected: {e}");
        assert!(e.contains("write \"www\""), "unexpected: {e}");
    }

    #[test]
    fn dotless_fqdn_style_record_name_is_also_an_error() {
        let e = norm_err(&raw(Some("www.example.com"), "a", None, &["192.0.2.10"]));
        assert!(e.contains("looks like an FQDN"), "unexpected: {e}");
    }

    /// DNS is case-insensitive; without folding, this slipped past the FQDN guard and produced
    /// `WWW.EXAMPLE.COM.example.com.`
    #[test]
    fn an_uppercase_fqdn_record_name_is_still_an_error() {
        let e = norm_err(&raw(Some("WWW.EXAMPLE.COM"), "a", None, &["192.0.2.10"]));
        assert!(e.contains("looks like an FQDN"), "unexpected: {e}");
    }

    #[test]
    fn record_names_are_lowercased() {
        assert_eq!(
            norm(&raw(Some("WWW"), "a", None, &["192.0.2.10"]))
                .unwrap()
                .name,
            "www.example.com."
        );
    }

    /// Same fold, seen from the duplicate check: WWW and www are one record set, not two.
    #[test]
    fn names_differing_only_in_case_are_a_duplicate() {
        let e = zone_err(&[
            raw(Some("www"), "a", None, &["192.0.2.10"]),
            raw(Some("WWW"), "a", None, &["192.0.2.11"]),
        ]);
        assert!(e.contains("duplicate record set"), "unexpected: {e}");
    }

    /// A clamped TTL could never equal the declared one, so the record would churn forever.
    #[test]
    fn a_ttl_above_the_signed_maximum_is_an_error() {
        let e = norm_err(&raw(Some("www"), "a", Some(u32::MAX), &["192.0.2.10"]));
        assert!(e.contains("above the maximum"), "unexpected: {e}");
    }

    #[test]
    fn the_largest_legal_ttl_is_accepted() {
        let ttl = norm(&raw(
            Some("www"),
            "a",
            Some(i32::MAX as u32),
            &["192.0.2.10"],
        ))
        .unwrap()
        .ttl;
        assert_eq!(ttl, i32::MAX as u32);
    }

    #[test]
    fn at_sign_record_name_is_an_error() {
        let e = norm_err(&raw(Some("@"), "a", None, &["192.0.2.10"]));
        assert!(e.contains("omit \"name\""), "unexpected: {e}");
    }

    #[test]
    fn empty_record_name_is_an_error() {
        assert!(norm_err(&raw(Some(""), "a", None, &["192.0.2.10"])).contains("omit \"name\""));
    }

    #[test]
    fn a_name_with_whitespace_is_an_error() {
        assert!(norm_err(&raw(Some("a b"), "a", None, &["x"])).contains("not a valid label"));
    }

    // ---- type and ttl -----------------------------------------------------------------------

    #[test]
    fn type_is_uppercased() {
        assert_eq!(
            norm(&raw(Some("www"), "a", None, &["192.0.2.10"]))
                .unwrap()
                .rtype,
            "A"
        );
    }

    #[test]
    fn nonsense_type_is_an_error() {
        assert!(norm_err(&raw(Some("www"), "a/b", None, &["x"])).contains("not a DNS record type"));
    }

    #[test]
    fn missing_ttl_defaults() {
        assert_eq!(
            norm(&raw(Some("www"), "a", None, &["192.0.2.10"]))
                .unwrap()
                .ttl,
            900
        );
    }

    #[test]
    fn explicit_ttl_is_kept() {
        assert_eq!(
            norm(&raw(Some("www"), "a", Some(300), &["192.0.2.10"]))
                .unwrap()
                .ttl,
            300
        );
    }

    // ---- apex reservations and targets --------------------------------------------------------

    #[test]
    fn apex_soa_declaration_is_an_error() {
        assert!(norm_err(&raw(None, "soa", None, &["x"])).contains("Cloud DNS owns those"));
    }

    #[test]
    fn apex_ns_declaration_is_an_error() {
        assert!(norm_err(&raw(None, "ns", None, &["ns1.example.net."])).contains("apex NS"));
    }

    /// The reservation is apex-scoped: a delegation NS below the apex is ours to manage.
    #[test]
    fn non_apex_ns_is_allowed() {
        assert_eq!(
            norm(&raw(Some("sub"), "ns", None, &["ns1.example.net."]))
                .unwrap()
                .name,
            "sub.example.com."
        );
    }

    #[test]
    fn empty_target_is_an_error() {
        assert!(norm_err(&raw(Some("www"), "a", None, &[])).contains("has no \"target\""));
    }

    #[test]
    fn empty_target_entry_is_an_error() {
        assert!(norm_err(&raw(Some("www"), "a", None, &["  "])).contains("empty \"target\" entry"));
    }

    // ---- zone-level checks ---------------------------------------------------------------------

    #[test]
    fn duplicate_name_type_pair_is_an_error() {
        let e = zone_err(&[
            raw(None, "mx", None, &["10 a."]),
            raw(None, "mx", None, &["20 b."]),
        ]);
        assert!(
            e.contains("duplicate record set \"example.com./MX\""),
            "unexpected: {e}"
        );
        assert!(e.contains("spliced in twice"), "unexpected: {e}");
    }

    #[test]
    fn same_name_different_type_is_not_a_duplicate() {
        let zone = normalize_zone(
            "example.com",
            &[
                raw(Some("www"), "a", None, &["192.0.2.10"]),
                raw(Some("www"), "txt", None, &["x"]),
            ],
            &[],
            DEFAULT_TTL,
        )
        .expect("normalizes");
        assert_eq!(zone.rrsets.len(), 2);
    }

    #[test]
    fn declaring_an_acme_challenge_record_is_an_error() {
        let e = zone_err(&[raw(Some("_acme-challenge"), "txt", None, &["x"])]);
        assert!(
            e.contains("matches ignore rule \"_acme-challenge*\""),
            "unexpected: {e}"
        );
        assert!(
            e.contains("cannot be both managed and ignored"),
            "unexpected: {e}"
        );
    }

    #[test]
    fn declaring_a_record_matching_a_custom_ignore_rule_is_an_error() {
        let e = format!(
            "{:#}",
            normalize_zone(
                "example.com",
                &[raw(Some("legacy"), "a", None, &["192.0.2.10"])],
                &[RawIgnore {
                    name: "legacy".into(),
                    rtype: None
                }],
                DEFAULT_TTL,
            )
            .expect_err("should fail")
        );
        assert!(
            e.contains("matches ignore rule \"legacy\""),
            "unexpected: {e}"
        );
    }

    #[test]
    fn the_default_acme_rule_is_always_present() {
        let zone = normalize_zone("example.com", &[], &[], DEFAULT_TTL).expect("normalizes");
        assert!(zone.ignore.iter().any(|r| r.name == "_acme-challenge*"));
    }

    #[test]
    fn description_matches_the_zone() {
        let zone = normalize_zone("example.com", &[], &[], DEFAULT_TTL).expect("normalizes");
        assert_eq!(zone.description, "example.com zone");
    }

    // ---- build_desired ---------------------------------------------------------------------

    fn build(content: &str) -> Result<Desired> {
        let docs = parse_documents("test.yaml", content).expect("parses");
        build_desired(&docs, DEFAULT_TTL)
    }

    #[test]
    fn zone_declared_twice_is_an_error() {
        let e = format!(
            "{:#}",
            build("kind: zone\nzone: example.com\n---\nkind: zone\nzone: Example.com.\n")
                .expect_err("should fail")
        );
        assert!(
            e.contains("zone example.com is declared twice"),
            "unexpected: {e}"
        );
        assert!(e.contains("document 0"), "unexpected: {e}");
        assert!(e.contains("document 1"), "unexpected: {e}");
    }

    #[test]
    fn diamond_snippet_duplication_is_caught_here() {
        let e = format!(
            "{:#}",
            build(
                "kind: snippets\n\
                 snippets:\n  \
                   leaf:\n    - {type: txt, target: [\"x\"]}\n  \
                   a:\n    - {use: leaf}\n  \
                   b:\n    - {use: leaf}\n\
                 ---\n\
                 kind: zone\nzone: example.com\nrecords:\n  - {use: a}\n  - {use: b}\n"
            )
            .expect_err("should fail")
        );
        assert!(e.contains("duplicate record set"), "unexpected: {e}");
    }

    #[test]
    fn end_to_end_example_builds_the_expected_desired() {
        let desired = build(
            "kind: snippets\n\
             snippets:\n  \
               mail-mx:\n    - {type: mx, ttl: 300, target: [\"10 mx1.mail.example.net.\"]}\n  \
               spf-dmarc:\n    \
                 - {type: txt, target: ['\"v=spf1 -all\"']}\n    \
                 - {name: _dmarc, type: txt, target: ['\"v=DMARC1; p=reject;\"']}\n  \
               mail-stack:\n    - {use: mail-mx}\n    - {use: spf-dmarc}\n\
             ---\n\
             kind: zone\n\
             zone: example.com\n\
             records:\n  \
               - {use: mail-stack}\n  \
               - {name: www, type: a, target: [192.0.2.10]}\n",
        )
        .expect("builds");

        assert_eq!(desired.zones.len(), 1);
        let zone = &desired.zones["example.com."];
        assert_eq!(zone.resource_name, "example-com");
        assert_eq!(zone.description, "example.com zone");

        let keys: Vec<(&str, &str)> = zone
            .rrsets
            .keys()
            .map(|(n, t)| (n.as_str(), t.as_str()))
            .collect();
        assert_eq!(
            keys,
            vec![
                ("_dmarc.example.com.", "TXT"),
                ("example.com.", "MX"),
                ("example.com.", "TXT"),
                ("www.example.com.", "A"),
            ]
        );

        let mx = &zone.rrsets[&("example.com.".to_string(), "MX".to_string())];
        assert_eq!(mx.ttl, 300, "explicit snippet ttl survives");
        let www = &zone.rrsets[&("www.example.com.".to_string(), "A".to_string())];
        assert_eq!(www.ttl, 900, "unset ttl takes the default");
        assert_eq!(www.rrdatas, vec!["192.0.2.10".to_string()]);
    }
}
