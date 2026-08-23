use std::collections::BTreeMap;

/// (rrset FQDN with its trailing dot, UPPERCASE type) — the reconciliation identity.
pub type RrKey = (String, String);

/// A fully-normalized record set: one YAML record becomes exactly one of these.
///
/// `Ord` is derived so every collection iterates deterministically and diff output is stable,
/// which is what lets the pure functions be asserted against without sorting at each call site.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rrset {
    /// `www.example.com.`, or `example.com.` at the apex.
    pub name: String,
    pub rtype: String,
    pub ttl: u32,
    pub rrdatas: Vec<String>,
}

impl Rrset {
    pub fn key(&self) -> RrKey {
        (self.name.clone(), self.rtype.clone())
    }

    /// TTL plus rrdatas as an unordered set. Cloud DNS does not promise to return rrdatas in the
    /// order they were written, so comparing positionally would report a change on every run.
    pub fn same_content(&self, other: &Self) -> bool {
        if self.ttl != other.ttl || self.rrdatas.len() != other.rrdatas.len() {
            return false;
        }
        let (mut a, mut b) = (self.rrdatas.clone(), other.rrdatas.clone());
        a.sort_unstable();
        b.sort_unstable();
        a == b
    }
}

/// A record set this tool must not touch, because something else owns it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct IgnoreRule {
    /// Glob over the relative label, where `*` also matches dots.
    pub name: String,
    /// `None` matches any type.
    pub rtype: Option<String>,
}

impl IgnoreRule {
    pub fn matches(&self, label: &str, rtype: &str) -> bool {
        match &self.rtype {
            Some(t) if !t.eq_ignore_ascii_case(rtype) => false,
            _ => glob_match(&self.name, label),
        }
    }
}

/// ACME DNS-01 solvers create and delete `_acme-challenge` TXT records under their own identity
/// while a certificate is being issued. Deleting one mid-flight fails the challenge, so this rule
/// is always in force and cannot be switched off.
pub fn default_ignore_rules() -> Vec<IgnoreRule> {
    vec![IgnoreRule {
        name: "_acme-challenge*".to_string(),
        rtype: Some("TXT".to_string()),
    }]
}

/// `*` matches any run of characters, dots included. A single wildcard form is deliberate: a glob
/// that stopped at a dot could not cover `_acme-challenge.foo` and `_acme-challenge` at once,
/// which is exactly what an ACME rule has to do.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // Position of the last `*` seen, and how much of `text` it had consumed at that point.
    let mut star: Option<usize> = None;
    let mut backtrack = 0usize;

    while ti < t.len() {
        if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            backtrack = ti;
            pi += 1;
        } else if let Some(s) = star {
            // Let the star swallow one more character and retry the rest of the pattern.
            backtrack += 1;
            ti = backtrack;
            pi = s + 1;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// The apex SOA and NS, which Cloud DNS creates with a zone and refuses to let go of: deleting
/// either is rejected with "a zone must contain exactly one resource record set of type 'SOA' at
/// the apex". A zone delete takes them with it; nothing else may.
pub fn is_apex_owned(rrset: &Rrset, dns_name: &str) -> bool {
    rrset.name == dns_name && (rrset.rtype == "SOA" || rrset.rtype == "NS")
}

/// The relative label of `fqdn` within `dns_name`: `""` at the apex, `None` if outside the zone.
pub fn label_of<'a>(fqdn: &'a str, dns_name: &str) -> Option<&'a str> {
    if fqdn == dns_name {
        return Some("");
    }
    fqdn.strip_suffix(dns_name)?.strip_suffix('.')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredZone {
    /// `example.com.` — with the trailing dot.
    pub dns_name: String,
    /// `example-com` — dots replaced by dashes.
    pub resource_name: String,
    pub description: String,
    pub rrsets: BTreeMap<RrKey, Rrset>,
    pub ignore: Vec<IgnoreRule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedZone {
    pub dns_name: String,
    /// Whatever GCP actually calls the zone, never the name we would have derived.
    pub resource_name: String,
    pub description: String,
    pub rrsets: BTreeMap<RrKey, Rrset>,
    /// Record sets carrying a routing policy (geo/WRR). They have no rrdatas, this tool has no
    /// vocabulary for them, and converging them would mean deleting them — so they are reported
    /// and left alone.
    pub unsupported: Vec<RrKey>,
}

/// Both maps are keyed by `dns_name` rather than `resource_name`: that is a zone's true identity,
/// so a zone someone created under a different resource name is recognized rather than deleted
/// and recreated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Desired {
    pub zones: BTreeMap<String, DesiredZone>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    pub zones: BTreeMap<String, ObservedZone>,
}

/// Everything that must happen inside one zone, as a single Cloud DNS change. An update is not a
/// separate variant — Cloud DNS models it as (delete old, add new) in the same atomic change, so
/// `updates` exists only for rendering the diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZoneChange {
    pub dns_name: String,
    pub resource_name: String,
    pub additions: Vec<Rrset>,
    pub deletions: Vec<Rrset>,
    pub updates: Vec<(Rrset, Rrset)>,
    /// The apex SOA as observed, paired with the same record set carrying the next serial.
    ///
    /// Kept out of `additions`/`deletions` so bookkeeping never shows up in the counts, and
    /// spliced into the API change only at submit time. `None` when the zone is unchanged, and
    /// also when the observed SOA could not be parsed — see `soa_bump_skipped`.
    pub soa: Option<(Rrset, Rrset)>,
    /// The zone changed but its SOA could not be parsed, so no serial bump was emitted. Reported
    /// as a warning and never fatal: a cosmetic serial must not block a DNS change.
    pub soa_bump_skipped: bool,
}

impl ZoneChange {
    pub fn is_empty(&self) -> bool {
        self.additions.is_empty() && self.deletions.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub zones_to_create: Vec<DesiredZone>,
    pub zones_to_delete: Vec<ObservedZone>,
    /// Only zones present in both desired and observed, and only those that actually change.
    /// A newly created zone gets its records through a change appended by `run` once it exists.
    pub zone_changes: Vec<ZoneChange>,
    /// Record sets the config declares that GCP holds under a routing policy. They are absent from
    /// the observed rrsets, so converging them would look like an addition and Cloud DNS would
    /// reject the whole change with 409 - forever. Reported instead, as (zone dns_name, key).
    pub conflicts: Vec<(String, RrKey)>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub zones_created: usize,
    pub zones_deleted: usize,
}

impl Plan {
    pub fn counts(&self) -> Counts {
        let mut c = Counts {
            zones_created: self.zones_to_create.len(),
            zones_deleted: self.zones_to_delete.len(),
            ..Counts::default()
        };
        for z in &self.zones_to_create {
            c.added += z.rrsets.len();
        }
        // A zone deletion takes its records with it; counting only the zone would report "0
        // removed" while a zone's entire contents are about to go. The apex SOA/NS are excluded:
        // they are Cloud DNS's, not ours to claim credit for.
        for z in &self.zones_to_delete {
            c.removed += z
                .rrsets
                .values()
                .filter(|r| !is_apex_owned(r, &z.dns_name))
                .count();
        }
        for zc in &self.zone_changes {
            c.updated += zc.updates.len();
            c.added += zc.additions.len() - zc.updates.len();
            c.removed += zc.deletions.len() - zc.updates.len();
        }
        c
    }

    pub fn is_empty(&self) -> bool {
        self.zones_to_create.is_empty()
            && self.zones_to_delete.is_empty()
            && self.zone_changes.is_empty()
            && self.conflicts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rr(name: &str, rtype: &str, ttl: u32, rrdatas: &[&str]) -> Rrset {
        Rrset {
            name: name.to_string(),
            rtype: rtype.to_string(),
            ttl,
            rrdatas: rrdatas.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn same_content_ignores_rrdata_order() {
        let a = rr("example.com.", "TXT", 300, &["a", "b"]);
        let b = rr("example.com.", "TXT", 300, &["b", "a"]);
        assert!(a.same_content(&b));
    }

    #[test]
    fn same_content_is_sensitive_to_ttl() {
        let a = rr("example.com.", "A", 300, &["192.0.2.10"]);
        let b = rr("example.com.", "A", 900, &["192.0.2.10"]);
        assert!(!a.same_content(&b));
    }

    #[test]
    fn same_content_is_sensitive_to_rrdata() {
        let a = rr("example.com.", "A", 300, &["192.0.2.10"]);
        let b = rr("example.com.", "A", 300, &["192.0.2.11"]);
        assert!(!a.same_content(&b));
        // A duplicated value is a different rrset, not the same one.
        let c = rr("example.com.", "A", 300, &["192.0.2.10", "192.0.2.10"]);
        assert!(!a.same_content(&c));
    }

    #[test]
    fn key_is_name_and_type() {
        assert_eq!(
            rr("www.example.com.", "A", 300, &["192.0.2.10"]).key(),
            ("www.example.com.".to_string(), "A".to_string())
        );
    }

    #[test]
    fn label_of_returns_the_relative_label() {
        assert_eq!(label_of("www.example.com.", "example.com."), Some("www"));
        assert_eq!(label_of("example.com.", "example.com."), Some(""));
        assert_eq!(
            label_of("_acme-challenge.foo.example.com.", "example.com."),
            Some("_acme-challenge.foo")
        );
        assert_eq!(label_of("www.example.net.", "example.com."), None);
    }

    #[test]
    fn ignore_glob_star_crosses_dots() {
        let rule = IgnoreRule {
            name: "_acme-challenge*".to_string(),
            rtype: Some("TXT".to_string()),
        };
        assert!(rule.matches("_acme-challenge", "TXT"));
        assert!(rule.matches("_acme-challenge.foo", "TXT"));
        assert!(rule.matches("_acme-challenge.foo.bar", "TXT"));
        // Type is part of the rule.
        assert!(!rule.matches("_acme-challenge", "A"));
        // And a label that merely contains the string is not a match.
        assert!(!rule.matches("not_acme-challenge", "TXT"));
        assert!(!rule.matches("www", "TXT"));
    }

    #[test]
    fn ignore_without_a_type_matches_any_type() {
        let rule = IgnoreRule {
            name: "_dnsauth".to_string(),
            rtype: None,
        };
        assert!(rule.matches("_dnsauth", "TXT"));
        assert!(rule.matches("_dnsauth", "CNAME"));
        assert!(!rule.matches("_dnsauthx", "TXT"));
    }

    #[test]
    fn ignore_type_match_is_case_insensitive() {
        let rule = IgnoreRule {
            name: "x".to_string(),
            rtype: Some("txt".to_string()),
        };
        assert!(rule.matches("x", "TXT"));
    }

    #[test]
    fn glob_matches_bare_star_and_literals() {
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything.at.all"));
        assert!(glob_match("www", "www"));
        assert!(!glob_match("www", "www2"));
        assert!(!glob_match("www", "ww"));
        assert!(glob_match("a*c", "abc"));
        assert!(glob_match("a*c", "ac"));
        assert!(!glob_match("a*c", "abd"));
        // Backtracking: the first star must not swallow the tail the pattern still needs.
        assert!(glob_match("*.example", "a.b.example"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
    }

    #[test]
    fn counts_separate_added_updated_and_removed() {
        let plan = Plan {
            zones_to_create: vec![],
            zones_to_delete: vec![],
            zone_changes: vec![ZoneChange {
                dns_name: "example.com.".to_string(),
                resource_name: "example-com".to_string(),
                // One pure addition, plus the new half of one update.
                additions: vec![
                    rr("new.example.com.", "A", 300, &["192.0.2.10"]),
                    rr("www.example.com.", "A", 900, &["192.0.2.11"]),
                ],
                // One pure deletion, plus the old half of that same update.
                deletions: vec![
                    rr("old.example.com.", "TXT", 300, &["x"]),
                    rr("www.example.com.", "A", 300, &["192.0.2.11"]),
                ],
                updates: vec![(
                    rr("www.example.com.", "A", 300, &["192.0.2.11"]),
                    rr("www.example.com.", "A", 900, &["192.0.2.11"]),
                )],
                soa: None,
                soa_bump_skipped: false,
            }],
            conflicts: vec![],
        };
        let c = plan.counts();
        assert_eq!(c.added, 1);
        assert_eq!(c.updated, 1);
        assert_eq!(c.removed, 1);
    }
}
