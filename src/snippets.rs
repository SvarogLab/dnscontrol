use crate::config::{DocRef, Document, Entry, RawRecord};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};

/// Every snippet in the tree, by name. Cross-file by construction: the registry is built from all
/// documents before any reference is resolved, so declaration order and file layout do not matter.
pub type Registry = BTreeMap<String, (DocRef, Vec<Entry>)>;

pub fn collect_snippets(docs: &[(DocRef, Document)]) -> Result<Registry> {
    let mut registry = Registry::new();
    for (at, doc) in docs {
        let Document::Snippets(snippets) = doc else {
            continue;
        };
        for (name, entries) in snippets {
            if let Some((first, _)) = registry.get(name) {
                bail!("snippet \"{name}\" is defined twice: {first} and {at}");
            }
            registry.insert(name.clone(), (at.clone(), entries.clone()));
        }
    }
    Ok(registry)
}

/// Splices every `use:` into a flat record list, depth-first in declaration order, so the result
/// reads exactly as the YAML would if it had been written out by hand.
///
/// `origin` names the caller for error messages, e.g. `zone example.com`.
pub fn expand(entries: &[Entry], registry: &Registry, origin: &str) -> Result<Vec<RawRecord>> {
    let mut out = Vec::new();
    let mut path = Vec::new();
    let mut active = BTreeSet::new();
    expand_into(entries, registry, origin, &mut path, &mut active, &mut out)?;
    Ok(out)
}

fn expand_into(
    entries: &[Entry],
    registry: &Registry,
    origin: &str,
    path: &mut Vec<String>,
    active: &mut BTreeSet<String>,
    out: &mut Vec<RawRecord>,
) -> Result<()> {
    for entry in entries {
        match entry {
            Entry::Record(record) => out.push(record.clone()),
            Entry::Use(name) => {
                // Cycle detection is exact — `active` holds precisely the snippets currently on
                // the stack — so no depth limit is needed.
                if active.contains(name) {
                    let mut chain = path.clone();
                    chain.push(name.clone());
                    bail!("snippet cycle detected: {}", chain.join(" -> "));
                }
                let Some((_, body)) = registry.get(name) else {
                    let known: Vec<&str> = registry.keys().map(String::as_str).collect();
                    let known = if known.is_empty() {
                        "none are defined".to_string()
                    } else {
                        format!("known snippets: {}", known.join(", "))
                    };
                    bail!("unknown snippet \"{name}\" referenced from {origin}; {known}");
                };

                active.insert(name.clone());
                path.push(name.clone());
                let nested_origin = format!("snippet {name}");
                expand_into(body, registry, &nested_origin, path, active, out)?;
                path.pop();
                active.remove(name);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(index: usize) -> DocRef {
        DocRef {
            file: "test.yaml".to_string(),
            index,
        }
    }

    fn record(name: &str) -> Entry {
        Entry::Record(RawRecord {
            name: Some(name.to_string()),
            rtype: "a".to_string(),
            ttl: None,
            target: vec!["192.0.2.10".to_string()],
        })
    }

    fn snippets(index: usize, defs: &[(&str, Vec<Entry>)]) -> (DocRef, Document) {
        let map = defs
            .iter()
            .map(|(n, e)| (n.to_string(), e.clone()))
            .collect();
        (at(index), Document::Snippets(map))
    }

    fn names(records: &[RawRecord]) -> Vec<&str> {
        records
            .iter()
            .map(|r| r.name.as_deref().unwrap_or("@"))
            .collect()
    }

    fn registry_of(defs: &[(&str, Vec<Entry>)]) -> Registry {
        collect_snippets(&[snippets(0, defs)]).expect("collects")
    }

    #[test]
    fn collects_snippets_from_multiple_documents() {
        let docs = vec![
            snippets(0, &[("a", vec![record("a1")])]),
            snippets(1, &[("b", vec![record("b1")])]),
            (
                at(2),
                Document::Zone {
                    zone: "example.com".to_string(),
                    records: vec![],
                    ignore: vec![],
                },
            ),
        ];
        let registry = collect_snippets(&docs).expect("collects");
        assert_eq!(
            registry.keys().collect::<Vec<_>>(),
            vec![&"a".to_string(), &"b".to_string()]
        );
    }

    #[test]
    fn duplicate_snippet_name_across_documents_is_an_error() {
        let docs = vec![
            snippets(0, &[("mail-mx", vec![record("a")])]),
            snippets(1, &[("mail-mx", vec![record("b")])]),
        ];
        let e = format!("{:#}", collect_snippets(&docs).expect_err("should fail"));
        assert!(e.contains("defined twice"), "unexpected: {e}");
        assert!(e.contains("test.yaml document 0"), "unexpected: {e}");
        assert!(e.contains("test.yaml document 1"), "unexpected: {e}");
    }

    #[test]
    fn expands_a_flat_snippet() {
        let registry = registry_of(&[("mx", vec![record("m1"), record("m2")])]);
        let got = expand(&[Entry::Use("mx".into())], &registry, "zone example.com").expect("ok");
        assert_eq!(names(&got), vec!["m1", "m2"]);
    }

    #[test]
    fn expands_a_snippet_that_uses_snippets() {
        let registry = registry_of(&[
            ("mx", vec![record("m1")]),
            ("spf", vec![record("s1"), record("s2")]),
            (
                "stack",
                vec![Entry::Use("mx".into()), Entry::Use("spf".into())],
            ),
        ]);
        let got = expand(&[Entry::Use("stack".into())], &registry, "zone example.com").expect("ok");
        assert_eq!(names(&got), vec!["m1", "s1", "s2"]);
    }

    #[test]
    fn preserves_declaration_order_when_splicing() {
        let registry = registry_of(&[("mx", vec![record("m1")])]);
        let got = expand(
            &[record("first"), Entry::Use("mx".into()), record("last")],
            &registry,
            "zone example.com",
        )
        .expect("ok");
        assert_eq!(names(&got), vec!["first", "m1", "last"]);
    }

    #[test]
    fn unknown_snippet_name_is_an_error_listing_known_names() {
        let registry = registry_of(&[("mail-mx", vec![]), ("spf-dmarc", vec![])]);
        let e = format!(
            "{:#}",
            expand(
                &[Entry::Use("mail-stak".into())],
                &registry,
                "zone example.com"
            )
            .expect_err("should fail")
        );
        assert!(
            e.contains("unknown snippet \"mail-stak\""),
            "unexpected: {e}"
        );
        assert!(
            e.contains("referenced from zone example.com"),
            "unexpected: {e}"
        );
        assert!(
            e.contains("known snippets: mail-mx, spf-dmarc"),
            "unexpected: {e}"
        );
    }

    #[test]
    fn a_nested_unknown_reference_names_the_snippet_it_came_from() {
        let registry = registry_of(&[("stack", vec![Entry::Use("missing".into())])]);
        let e = format!(
            "{:#}",
            expand(&[Entry::Use("stack".into())], &registry, "zone example.com")
                .expect_err("should fail")
        );
        assert!(
            e.contains("referenced from snippet stack"),
            "unexpected: {e}"
        );
    }

    #[test]
    fn direct_self_reference_is_a_cycle() {
        let registry = registry_of(&[("loop", vec![Entry::Use("loop".into())])]);
        let e = format!(
            "{:#}",
            expand(&[Entry::Use("loop".into())], &registry, "zone example.com")
                .expect_err("should fail")
        );
        assert!(
            e.contains("snippet cycle detected: loop -> loop"),
            "unexpected: {e}"
        );
    }

    #[test]
    fn two_node_cycle_is_detected() {
        let registry = registry_of(&[
            ("a", vec![Entry::Use("b".into())]),
            ("b", vec![Entry::Use("a".into())]),
        ]);
        let e = format!(
            "{:#}",
            expand(&[Entry::Use("a".into())], &registry, "zone example.com")
                .expect_err("should fail")
        );
        assert!(
            e.contains("snippet cycle detected: a -> b -> a"),
            "unexpected: {e}"
        );
    }

    #[test]
    fn three_node_cycle_is_detected() {
        let registry = registry_of(&[
            ("a", vec![Entry::Use("b".into())]),
            ("b", vec![Entry::Use("c".into())]),
            ("c", vec![Entry::Use("a".into())]),
        ]);
        let e = format!(
            "{:#}",
            expand(&[Entry::Use("a".into())], &registry, "zone example.com")
                .expect_err("should fail")
        );
        assert!(
            e.contains("snippet cycle detected: a -> b -> c -> a"),
            "unexpected: {e}"
        );
    }

    /// A diamond is not a cycle: `top` uses `a` and `b`, both of which use `leaf`. Expansion
    /// duplicates leaf's records, which normalization then rejects as a duplicate record set.
    #[test]
    fn diamond_reference_is_not_a_cycle() {
        let registry = registry_of(&[
            ("leaf", vec![record("leaf1")]),
            ("a", vec![Entry::Use("leaf".into())]),
            ("b", vec![Entry::Use("leaf".into())]),
            ("top", vec![Entry::Use("a".into()), Entry::Use("b".into())]),
        ]);
        let got = expand(&[Entry::Use("top".into())], &registry, "zone example.com").expect("ok");
        assert_eq!(names(&got), vec!["leaf1", "leaf1"]);
    }

    #[test]
    fn a_snippet_used_twice_in_sequence_is_not_a_cycle() {
        let registry = registry_of(&[("mx", vec![record("m1")])]);
        let got = expand(
            &[Entry::Use("mx".into()), Entry::Use("mx".into())],
            &registry,
            "zone example.com",
        )
        .expect("ok");
        assert_eq!(names(&got), vec!["m1", "m1"]);
    }

    #[test]
    fn expanding_an_empty_list_yields_nothing() {
        let registry = registry_of(&[]);
        assert!(
            expand(&[], &registry, "zone example.com")
                .expect("ok")
                .is_empty()
        );
    }
}
