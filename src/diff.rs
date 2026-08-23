use crate::model::{Plan, RrKey, Rrset, is_apex_owned};
use std::collections::BTreeMap;

const NAME_WIDTH: usize = 34;
const TYPE_WIDTH: usize = 6;
const TTL_WIDTH: usize = 6;

/// What happened to one record set, for rendering only.
enum Entry<'a> {
    Added(&'a Rrset),
    Updated(&'a Rrset, &'a Rrset),
    Removed(&'a Rrset),
}

/// Renders a plan as `ansible-playbook --diff`-style text.
///
/// Every line begins with `+`, `~`, `-`, a space, or `P`, which is what lets the caller colorize
/// by first character.
pub fn render(plan: &Plan) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    for zone in &plan.zones_to_create {
        out.push(format!(
            "+ zone {} ({}) \"{}\"",
            zone.resource_name, zone.dns_name, zone.description
        ));
        for rrset in zone.rrsets.values() {
            push_rrset(&mut out, '+', rrset);
        }
        out.push(String::new());
    }

    for change in &plan.zone_changes {
        out.push(format!(
            "  zone {} ({})",
            change.resource_name, change.dns_name
        ));

        // One pass over a single ordered map, so the output is stable no matter what order the
        // reconciler happened to push additions and deletions in.
        let updated: BTreeMap<RrKey, (&Rrset, &Rrset)> = change
            .updates
            .iter()
            .map(|(old, new)| (old.key(), (old, new)))
            .collect();
        let mut entries: BTreeMap<RrKey, Entry> = BTreeMap::new();
        for rrset in &change.additions {
            entries.entry(rrset.key()).or_insert(Entry::Added(rrset));
        }
        for rrset in &change.deletions {
            entries.entry(rrset.key()).or_insert(Entry::Removed(rrset));
        }
        for (key, (old, new)) in updated {
            entries.insert(key, Entry::Updated(old, new));
        }

        for entry in entries.values() {
            match entry {
                Entry::Added(rrset) => push_rrset(&mut out, '+', rrset),
                Entry::Removed(rrset) => push_rrset(&mut out, '-', rrset),
                Entry::Updated(old, new) => push_update(&mut out, old, new),
            }
        }
        if let Some((old, new)) = &change.soa {
            out.push(format!(
                "      soa serial: {} -> {}",
                serial(old).unwrap_or("?"),
                serial(new).unwrap_or("?")
            ));
        }
        out.push(String::new());
    }

    for zone in &plan.zones_to_delete {
        out.push(format!(
            "- zone {} ({}) \"{}\"",
            zone.resource_name, zone.dns_name, zone.description
        ));
        // Everything inside goes with the zone. For the one irreversible operation this tool
        // performs, showing only a header would hide exactly what is about to be lost. The apex
        // SOA/NS are omitted: Cloud DNS created them and takes them back on its own.
        for rrset in zone.rrsets.values() {
            if !is_apex_owned(rrset, &zone.dns_name) {
                push_rrset(&mut out, '-', rrset);
            }
        }
        out.push(String::new());
    }

    let c = plan.counts();
    out.push(format!(
        "PLAN: {} added, {} updated, {} removed, {} zone{} created, {} zone{} deleted",
        c.added,
        c.updated,
        c.removed,
        c.zones_created,
        plural(c.zones_created),
        c.zones_deleted,
        plural(c.zones_deleted),
    ));
    out
}

fn serial(soa: &Rrset) -> Option<&str> {
    soa.rrdatas.first()?.split_whitespace().nth(2)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_RESET: &str = "\x1b[0m";

/// Colorizes one line by its leading marker. Anything else passes through unstyled rather than
/// guessing.
fn colorize_diff_line(line: &str) -> String {
    let color = match line.chars().next() {
        Some('+') => ANSI_GREEN,
        Some('-') => ANSI_RED,
        Some('~') => ANSI_YELLOW,
        _ => return line.to_string(),
    };
    format!("{color}{line}{ANSI_RESET}")
}

/// Color only when stdout is a real terminal, and not when `NO_COLOR` is set (no-color.org).
fn use_color() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// Prints a rendered plan to stdout. Logs go to stderr, so the two never interleave.
pub fn print(lines: &[String]) {
    let color = use_color();
    for line in lines {
        if color {
            println!("{}", colorize_diff_line(line));
        } else {
            println!("{line}");
        }
    }
}

/// One line per rrdata, so a `+`/`-` line is always exactly one datum on the wire.
fn push_rrset(out: &mut Vec<String>, marker: char, rrset: &Rrset) {
    for rrdata in &rrset.rrdatas {
        out.push(format!(
            "{marker}   {:<NAME_WIDTH$}{:<TYPE_WIDTH$}{:<TTL_WIDTH$}{rrdata}",
            rrset.name, rrset.rtype, rrset.ttl
        ));
    }
    if rrset.rrdatas.is_empty() {
        out.push(format!(
            "{marker}   {:<NAME_WIDTH$}{:<TYPE_WIDTH$}{}",
            rrset.name, rrset.rtype, rrset.ttl
        ));
    }
}

fn push_update(out: &mut Vec<String>, old: &Rrset, new: &Rrset) {
    out.push(format!("~   {:<NAME_WIDTH$}{}", old.name, old.rtype));
    if old.ttl != new.ttl {
        out.push(format!("      ttl:     {} -> {}", old.ttl, new.ttl));
    } else {
        out.push(format!("      ttl:     {}", old.ttl));
    }
    for rrdata in &old.rrdatas {
        if !new.rrdatas.contains(rrdata) {
            out.push(format!("      - {rrdata}"));
        }
    }
    for rrdata in &new.rrdatas {
        if !old.rrdatas.contains(rrdata) {
            out.push(format!("      + {rrdata}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DesiredZone, ObservedZone, ZoneChange};
    use std::collections::BTreeMap;

    fn rr(name: &str, rtype: &str, ttl: u32, rrdatas: &[&str]) -> Rrset {
        Rrset {
            name: name.to_string(),
            rtype: rtype.to_string(),
            ttl,
            rrdatas: rrdatas.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn desired_zone(rrsets: &[Rrset]) -> DesiredZone {
        DesiredZone {
            dns_name: "example.com.".to_string(),
            resource_name: "example-com".to_string(),
            description: "example.com zone".to_string(),
            rrsets: rrsets.iter().map(|r| (r.key(), r.clone())).collect(),
            ignore: vec![],
        }
    }

    fn observed_zone() -> ObservedZone {
        ObservedZone {
            dns_name: "legacy.example.org.".to_string(),
            resource_name: "legacy-example-org".to_string(),
            description: "legacy.example.org zone".to_string(),
            rrsets: BTreeMap::new(),
            unsupported: vec![],
        }
    }

    fn last(lines: &[String]) -> &str {
        lines.last().expect("at least the PLAN line")
    }

    #[test]
    fn an_empty_plan_renders_only_the_summary() {
        let lines = render(&Plan::default());
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0],
            "PLAN: 0 added, 0 updated, 0 removed, 0 zones created, 0 zones deleted"
        );
    }

    #[test]
    fn a_created_zone_renders_its_header_and_records() {
        let lines = render(&Plan {
            zones_to_create: vec![desired_zone(&[
                rr("example.com.", "MX", 300, &["10 mx1.mail.example.net."]),
                rr("www.example.com.", "A", 900, &["192.0.2.10"]),
            ])],
            ..Plan::default()
        });
        assert_eq!(
            lines[0],
            "+ zone example-com (example.com.) \"example.com zone\""
        );
        assert!(
            lines[1].starts_with("+   example.com."),
            "got {:?}",
            lines[1]
        );
        assert!(lines[1].ends_with("10 mx1.mail.example.net."));
        assert!(lines[2].contains("www.example.com.") && lines[2].ends_with("192.0.2.10"));
        assert_eq!(
            last(&lines),
            "PLAN: 2 added, 0 updated, 0 removed, 1 zone created, 0 zones deleted"
        );
    }

    #[test]
    fn a_deleted_zone_renders_a_minus_header() {
        let lines = render(&Plan {
            zones_to_delete: vec![observed_zone()],
            ..Plan::default()
        });
        assert_eq!(
            lines[0],
            "- zone legacy-example-org (legacy.example.org.) \"legacy.example.org zone\""
        );
        assert!(last(&lines).contains("1 zone deleted"));
    }

    fn doomed_zone() -> ObservedZone {
        let records = [
            rr(
                "legacy.example.org.",
                "SOA",
                21600,
                &["ns1.example.net. h.example.net. 1 2 3 4 5"],
            ),
            rr("legacy.example.org.", "NS", 21600, &["ns1.example.net."]),
            rr("www.legacy.example.org.", "A", 300, &["192.0.2.10"]),
            rr("legacy.example.org.", "TXT", 300, &["\"v=spf1 -all\""]),
        ];
        ObservedZone {
            rrsets: records.iter().map(|r| (r.key(), r.clone())).collect(),
            ..observed_zone()
        }
    }

    /// The one irreversible operation this tool performs: a header alone would hide what is lost.
    #[test]
    fn a_deleted_zone_lists_the_records_going_with_it() {
        let lines = render(&Plan {
            zones_to_delete: vec![doomed_zone()],
            ..Plan::default()
        });
        let removed: Vec<&String> = lines.iter().filter(|l| l.starts_with("-   ")).collect();
        assert_eq!(removed.len(), 2, "the two real records: {removed:?}");
        assert!(
            removed
                .iter()
                .any(|l| l.contains("www.legacy.example.org."))
        );
        assert!(removed.iter().any(|l| l.contains("v=spf1 -all")));
        assert!(
            !lines
                .iter()
                .any(|l| l.contains(" SOA ") || l.contains(" NS ")),
            "the apex SOA/NS belong to Cloud DNS and go with the zone itself"
        );
    }

    #[test]
    fn a_deleted_zones_records_are_counted_as_removed() {
        let lines = render(&Plan {
            zones_to_delete: vec![doomed_zone()],
            ..Plan::default()
        });
        assert!(
            last(&lines).contains("2 removed"),
            "reporting 0 removed while a zone's contents vanish is a lie: {}",
            last(&lines)
        );
    }

    #[test]
    fn a_multi_rrdata_rrset_gets_one_line_per_datum() {
        let lines = render(&Plan {
            zones_to_create: vec![desired_zone(&[rr(
                "example.com.",
                "MX",
                300,
                &["10 mx1.mail.example.net.", "20 mx2.mail.example.net."],
            )])],
            ..Plan::default()
        });
        let marked: Vec<&String> = lines.iter().filter(|l| l.starts_with('+')).collect();
        assert_eq!(
            marked.len(),
            3,
            "header plus one line per rrdata: {marked:?}"
        );
    }

    #[test]
    fn an_update_renders_ttl_and_rrdata_deltas() {
        let old = rr("www.example.com.", "A", 300, &["192.0.2.10"]);
        let new = rr("www.example.com.", "A", 900, &["192.0.2.11"]);
        let lines = render(&Plan {
            zone_changes: vec![ZoneChange {
                dns_name: "example.com.".to_string(),
                resource_name: "example-com".to_string(),
                additions: vec![new.clone()],
                deletions: vec![old.clone()],
                updates: vec![(old, new)],
                soa: None,
                soa_bump_skipped: false,
            }],
            ..Plan::default()
        });
        assert_eq!(lines[0], "  zone example-com (example.com.)");
        assert!(
            lines[1].starts_with("~   www.example.com."),
            "got {:?}",
            lines[1]
        );
        assert_eq!(lines[2], "      ttl:     300 -> 900");
        assert_eq!(lines[3], "      - 192.0.2.10");
        assert_eq!(lines[4], "      + 192.0.2.11");
        assert!(last(&lines).contains("0 added, 1 updated, 0 removed"));
    }

    #[test]
    fn an_update_that_only_changes_rrdata_still_shows_the_ttl() {
        let old = rr("www.example.com.", "A", 300, &["192.0.2.10"]);
        let new = rr("www.example.com.", "A", 300, &["192.0.2.11"]);
        let lines = render(&Plan {
            zone_changes: vec![ZoneChange {
                dns_name: "example.com.".to_string(),
                resource_name: "example-com".to_string(),
                additions: vec![new.clone()],
                deletions: vec![old.clone()],
                updates: vec![(old, new)],
                soa: None,
                soa_bump_skipped: false,
            }],
            ..Plan::default()
        });
        assert_eq!(
            lines[2], "      ttl:     300",
            "identity is shown without an arrow"
        );
    }

    #[test]
    fn additions_and_deletions_in_one_zone_are_ordered_by_key() {
        let lines = render(&Plan {
            zone_changes: vec![ZoneChange {
                dns_name: "example.com.".to_string(),
                resource_name: "example-com".to_string(),
                additions: vec![rr("zzz.example.com.", "A", 300, &["192.0.2.10"])],
                deletions: vec![rr("aaa.example.com.", "TXT", 300, &["x"])],
                updates: vec![],
                soa: None,
                soa_bump_skipped: false,
            }],
            ..Plan::default()
        });
        assert!(lines[1].starts_with("-   aaa."), "got {:?}", lines[1]);
        assert!(lines[2].starts_with("+   zzz."), "got {:?}", lines[2]);
    }

    /// The invariant the caller's colorizer relies on.
    #[test]
    fn every_line_starts_with_a_known_marker() {
        let old = rr("www.example.com.", "A", 300, &["192.0.2.10"]);
        let new = rr("www.example.com.", "A", 900, &["192.0.2.11"]);
        let lines = render(&Plan {
            zones_to_create: vec![desired_zone(&[rr("example.com.", "TXT", 300, &["x"])])],
            zones_to_delete: vec![observed_zone()],
            zone_changes: vec![ZoneChange {
                dns_name: "example.com.".to_string(),
                resource_name: "example-com".to_string(),
                additions: vec![new.clone()],
                deletions: vec![old.clone()],
                updates: vec![(old, new)],
                soa: None,
                soa_bump_skipped: false,
            }],
            conflicts: vec![],
        });
        for line in &lines {
            let first = line.chars().next().unwrap_or(' ');
            assert!(
                matches!(first, '+' | '~' | '-' | ' ' | 'P'),
                "line does not start with a known marker: {line:?}"
            );
        }
    }

    #[test]
    fn colorizes_add_update_remove_by_leading_marker() {
        assert_eq!(
            colorize_diff_line("+ zone example-com"),
            format!("{ANSI_GREEN}+ zone example-com{ANSI_RESET}")
        );
        assert_eq!(
            colorize_diff_line("~   www.example.com."),
            format!("{ANSI_YELLOW}~   www.example.com.{ANSI_RESET}")
        );
        assert_eq!(
            colorize_diff_line("- zone example-com"),
            format!("{ANSI_RED}- zone example-com{ANSI_RESET}")
        );
    }

    #[test]
    fn passes_through_unrecognized_lines_unstyled() {
        assert_eq!(
            colorize_diff_line("  zone example-com"),
            "  zone example-com"
        );
        assert_eq!(colorize_diff_line("PLAN: 0 added"), "PLAN: 0 added");
        assert_eq!(colorize_diff_line(""), "");
    }

    #[test]
    fn the_summary_singularizes_one_zone() {
        let lines = render(&Plan {
            zones_to_create: vec![desired_zone(&[])],
            zones_to_delete: vec![observed_zone()],
            ..Plan::default()
        });
        assert!(
            last(&lines).ends_with("1 zone created, 1 zone deleted"),
            "got {:?}",
            last(&lines)
        );
    }
}
