use crate::model::{
    Desired, Observed, ObservedZone, Plan, RrKey, Rrset, ZoneChange, is_apex_owned, label_of,
};

/// Diffs desired against observed state.
///
/// Infallible by construction: every way a configuration can be wrong was already rejected during
/// normalization. Deterministic because every collection it walks is ordered.
pub fn plan(desired: &Desired, observed: &Observed, delete_undeclared_zones: bool) -> Plan {
    let mut out = Plan::default();

    for (dns_name, zone) in &desired.zones {
        if !observed.zones.contains_key(dns_name) {
            out.zones_to_create.push(zone.clone());
        }
    }

    if delete_undeclared_zones {
        for (dns_name, zone) in &observed.zones {
            if !desired.zones.contains_key(dns_name) {
                out.zones_to_delete.push(zone.clone());
            }
        }
    }

    for (dns_name, want) in &desired.zones {
        let Some(have) = observed.zones.get(dns_name) else {
            continue;
        };

        let mut change = ZoneChange {
            dns_name: dns_name.clone(),
            // Always the name GCP actually uses, never the one we would have derived.
            resource_name: have.resource_name.clone(),
            ..ZoneChange::default()
        };

        for (key, new) in &want.rrsets {
            // GCP holds this one under a routing policy, so it is missing from have.rrsets. Adding
            // it would be a 409 on every run; report the clash and let `run` refuse the whole plan.
            if have.unsupported.contains(key) {
                out.conflicts.push((dns_name.clone(), key.clone()));
                continue;
            }
            match have.rrsets.get(key) {
                None => change.additions.push(new.clone()),
                Some(old) if old.same_content(new) => {}
                Some(old) => {
                    // Cloud DNS requires a deletion to match the stored record set exactly, so the
                    // observed value goes on the wire verbatim - a re-normalized copy would 412.
                    change.deletions.push(old.clone());
                    change.additions.push(new.clone());
                    change.updates.push((old.clone(), new.clone()));
                }
            }
        }

        for (key, old) in &have.rrsets {
            if want.rrsets.contains_key(key) || is_protected(key, have, want) {
                continue;
            }
            change.deletions.push(old.clone());
        }

        if !change.is_empty() {
            // Cloud DNS does not touch the serial on changes.create - verified against the live
            // service by adding a record and re-reading the SOA - so the bump is ours to make. It
            // rides in the same atomic change as the records it describes.
            match have.rrsets.get(&(have.dns_name.clone(), "SOA".to_string())) {
                Some(soa) => match bump_soa(soa) {
                    Some(bumped) => change.soa = Some((soa.clone(), bumped)),
                    None => change.soa_bump_skipped = true,
                },
                None => change.soa_bump_skipped = true,
            }
            out.zone_changes.push(change);
        }
    }

    out
}

/// Record sets this tool must never delete: the apex SOA and NS that Cloud DNS owns, record sets
/// carrying a routing policy we cannot express, and anything an ignore rule claims.
fn is_protected(key: &RrKey, have: &ObservedZone, want: &crate::model::DesiredZone) -> bool {
    let (name, rtype) = key;

    // Load-bearing, not politeness. Normalization refuses to let a config declare these, so without
    // this they would be undeclared-and-therefore-deleted on every run - and Cloud DNS rejects that
    // with "a zone must contain exactly one resource record set of type 'SOA' at the apex" (HTTP
    // 400, measured). Since a zone converges as one atomic change, that single rejection would sink
    // every other record with it. It is also why a serial bump has to be a delete/add pair: either
    // half on its own breaks the same invariant.
    if let Some(rrset) = have.rrsets.get(key)
        && is_apex_owned(rrset, &have.dns_name)
    {
        return true;
    }
    if have.unsupported.contains(key) {
        return true;
    }
    let label = label_of(name, &have.dns_name).unwrap_or(name);
    want.ignore.iter().any(|rule| rule.matches(label, rtype))
}

/// Bumps the serial in an apex SOA record set.
///
/// The rdata is a single string of seven space-separated fields, of which index 2 is the serial.
/// Returns `None` for anything that does not look like that, so an unparseable SOA is reported and
/// skipped rather than aborting a run - a cosmetic serial must not block a DNS change.
pub fn bump_soa(soa: &Rrset) -> Option<Rrset> {
    let [rrdata] = soa.rrdatas.as_slice() else {
        return None;
    };
    let mut fields: Vec<&str> = rrdata.split_whitespace().collect();
    if fields.len() != 7 {
        return None;
    }
    // RFC 1982 serial arithmetic: the space wraps rather than saturating.
    let bumped = fields[2].parse::<u32>().ok()?.wrapping_add(1).to_string();
    fields[2] = &bumped;
    Some(Rrset {
        rrdatas: vec![fields.join(" ")],
        ..soa.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DesiredZone, IgnoreRule, default_ignore_rules};
    use std::collections::BTreeMap;

    fn rr(name: &str, rtype: &str, ttl: u32, rrdatas: &[&str]) -> Rrset {
        Rrset {
            name: format!("{name}example.com."),
            rtype: rtype.to_string(),
            ttl,
            rrdatas: rrdatas.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn map(rrsets: &[Rrset]) -> BTreeMap<RrKey, Rrset> {
        rrsets.iter().map(|r| (r.key(), r.clone())).collect()
    }

    fn want(rrsets: &[Rrset]) -> Desired {
        want_with_ignore(rrsets, vec![])
    }

    fn want_with_ignore(rrsets: &[Rrset], extra: Vec<IgnoreRule>) -> Desired {
        let mut ignore = extra;
        ignore.extend(default_ignore_rules());
        let mut zones = BTreeMap::new();
        zones.insert(
            "example.com.".to_string(),
            DesiredZone {
                dns_name: "example.com.".to_string(),
                resource_name: "example-com".to_string(),
                description: "example.com zone".to_string(),
                rrsets: map(rrsets),
                ignore,
            },
        );
        Desired { zones }
    }

    fn have(rrsets: &[Rrset]) -> Observed {
        have_named("example-com", rrsets, vec![])
    }

    fn have_named(resource_name: &str, rrsets: &[Rrset], unsupported: Vec<RrKey>) -> Observed {
        let mut zones = BTreeMap::new();
        zones.insert(
            "example.com.".to_string(),
            ObservedZone {
                dns_name: "example.com.".to_string(),
                resource_name: resource_name.to_string(),
                description: "example.com zone".to_string(),
                rrsets: map(rrsets),
                unsupported,
            },
        );
        Observed { zones }
    }

    fn only_change(plan: &Plan) -> &ZoneChange {
        assert_eq!(
            plan.zone_changes.len(),
            1,
            "expected one zone change: {plan:?}"
        );
        &plan.zone_changes[0]
    }

    // ---- zones ------------------------------------------------------------------------------

    #[test]
    fn empty_desired_and_observed_is_an_empty_plan() {
        let p = plan(&Desired::default(), &Observed::default(), false);
        assert!(p.is_empty());
        assert_eq!(p.counts(), Default::default());
    }

    #[test]
    fn declared_missing_zone_is_created_with_all_its_rrsets() {
        let p = plan(
            &want(&[rr("www.", "A", 300, &["192.0.2.10"])]),
            &Observed::default(),
            false,
        );
        assert_eq!(p.zones_to_create.len(), 1);
        assert_eq!(p.zones_to_create[0].resource_name, "example-com");
        assert_eq!(p.zones_to_create[0].rrsets.len(), 1);
        assert!(
            p.zone_changes.is_empty(),
            "records ride along with the create"
        );
        assert_eq!(p.counts().zones_created, 1);
        assert_eq!(p.counts().added, 1);
    }

    #[test]
    fn undeclared_zone_is_kept_by_default() {
        let p = plan(&Desired::default(), &have(&[]), false);
        assert!(p.zones_to_delete.is_empty());
        assert!(p.is_empty());
    }

    #[test]
    fn undeclared_zone_is_deleted_only_when_asked() {
        let p = plan(&Desired::default(), &have(&[]), true);
        assert_eq!(p.zones_to_delete.len(), 1);
        assert_eq!(p.counts().zones_deleted, 1);
    }

    #[test]
    fn zone_matched_by_dns_name_uses_the_observed_resource_name() {
        let p = plan(
            &want(&[rr("www.", "A", 300, &["192.0.2.10"])]),
            &have_named("renamed-by-hand", &[], vec![]),
            false,
        );
        assert!(p.zones_to_create.is_empty(), "must not recreate the zone");
        assert_eq!(only_change(&p).resource_name, "renamed-by-hand");
    }

    // ---- record sets --------------------------------------------------------------------------

    #[test]
    fn identical_zone_produces_no_change() {
        let rrsets = [rr("www.", "A", 300, &["192.0.2.10"])];
        assert!(plan(&want(&rrsets), &have(&rrsets), false).is_empty());
    }

    #[test]
    fn new_rrset_is_an_addition() {
        let p = plan(
            &want(&[rr("www.", "A", 300, &["192.0.2.10"])]),
            &have(&[]),
            false,
        );
        let c = only_change(&p);
        assert_eq!(c.additions.len(), 1);
        assert!(c.deletions.is_empty());
        assert_eq!(p.counts().added, 1);
    }

    #[test]
    fn undeclared_rrset_is_a_deletion() {
        let p = plan(&want(&[]), &have(&[rr("old.", "TXT", 300, &["x"])]), false);
        let c = only_change(&p);
        assert_eq!(c.deletions.len(), 1);
        assert!(c.additions.is_empty());
        assert_eq!(p.counts().removed, 1);
    }

    #[test]
    fn ttl_only_change_is_an_update_not_a_delete() {
        let old = rr("www.", "A", 300, &["192.0.2.10"]);
        let new = rr("www.", "A", 900, &["192.0.2.10"]);
        let p = plan(
            &want(std::slice::from_ref(&new)),
            &have(std::slice::from_ref(&old)),
            false,
        );
        let c = only_change(&p);
        assert_eq!(c.additions, vec![new]);
        assert_eq!(
            c.deletions,
            vec![old.clone()],
            "deletion must be the observed value verbatim"
        );
        assert_eq!(c.updates.len(), 1);
        let counts = p.counts();
        assert_eq!((counts.added, counts.updated, counts.removed), (0, 1, 0));
    }

    #[test]
    fn rrdata_change_is_an_update() {
        let p = plan(
            &want(&[rr("www.", "A", 300, &["192.0.2.11"])]),
            &have(&[rr("www.", "A", 300, &["192.0.2.10"])]),
            false,
        );
        assert_eq!(only_change(&p).updates.len(), 1);
    }

    #[test]
    fn rrdata_reorder_is_not_a_change() {
        let p = plan(
            &want(&[rr("", "TXT", 300, &["a", "b"])]),
            &have(&[rr("", "TXT", 300, &["b", "a"])]),
            false,
        );
        assert!(
            p.is_empty(),
            "unordered rrdata must not produce a phantom update"
        );
    }

    #[test]
    fn same_name_different_type_are_independent_keys() {
        let p = plan(
            &want(&[rr("foo.", "A", 300, &["192.0.2.10"])]),
            &have(&[rr("foo.", "TXT", 300, &["x"])]),
            false,
        );
        let c = only_change(&p);
        assert_eq!(c.additions.len(), 1);
        assert_eq!(c.deletions.len(), 1);
        assert!(c.updates.is_empty());
    }

    // ---- protected record sets -----------------------------------------------------------------

    #[test]
    fn apex_soa_is_never_deleted() {
        let soa = Rrset {
            name: "example.com.".to_string(),
            rtype: "SOA".to_string(),
            ttl: 21600,
            rrdatas: vec![
                "ns1.example.net. hostmaster.example.net. 3 21600 3600 259200 300".into(),
            ],
        };
        assert!(plan(&want(&[]), &have(&[soa]), false).is_empty());
    }

    #[test]
    fn apex_ns_is_never_deleted() {
        let ns = rr("", "NS", 21600, &["ns1.example.net."]);
        assert!(plan(&want(&[]), &have(&[ns]), false).is_empty());
    }

    /// The reservation is apex-scoped: a delegation NS below the apex is ours, and goes if
    /// undeclared.
    #[test]
    fn non_apex_ns_delegation_is_deleted_when_undeclared() {
        let p = plan(
            &want(&[]),
            &have(&[rr("sub.", "NS", 300, &["ns1.example.net."])]),
            false,
        );
        assert_eq!(only_change(&p).deletions.len(), 1);
    }

    #[test]
    fn acme_challenge_txt_is_never_deleted() {
        let p = plan(
            &want(&[]),
            &have(&[
                rr("_acme-challenge.", "TXT", 60, &["token"]),
                rr("_acme-challenge.foo.", "TXT", 60, &["token"]),
            ]),
            false,
        );
        assert!(
            p.is_empty(),
            "an in-flight ACME challenge must survive a converge"
        );
    }

    /// The ACME rule is name-scoped, not a blanket exemption for the label.
    #[test]
    fn an_a_record_at_the_acme_label_is_still_deleted() {
        let p = plan(
            &want(&[]),
            &have(&[rr("_acme-challenge.", "A", 60, &["192.0.2.10"])]),
            false,
        );
        assert_eq!(only_change(&p).deletions.len(), 1);
    }

    #[test]
    fn a_custom_ignore_rule_protects_a_record() {
        let rule = IgnoreRule {
            name: "legacy*".to_string(),
            rtype: None,
        };
        let p = plan(
            &want_with_ignore(&[], vec![rule]),
            &have(&[rr("legacy.", "A", 300, &["192.0.2.10"])]),
            false,
        );
        assert!(p.is_empty());
    }

    #[test]
    fn rrset_with_a_routing_policy_is_never_deleted() {
        let geo = rr("geo.", "A", 300, &[]);
        let p = plan(
            &want(&[]),
            &have_named("example-com", std::slice::from_ref(&geo), vec![geo.key()]),
            false,
        );
        assert!(
            p.is_empty(),
            "a geo/WRR policy we cannot express must be left alone"
        );
    }

    /// Declaring a name GCP serves through a routing policy can never converge: the record is
    /// absent from the observed set, so it looks like an addition, and Cloud DNS answers 409 every
    /// single run. Report it instead of planning it.
    #[test]
    fn declaring_a_record_gcp_holds_under_a_routing_policy_is_a_conflict() {
        let geo = rr("geo.", "A", 300, &["192.0.2.10"]);
        let p = plan(
            &want(std::slice::from_ref(&geo)),
            &have_named("example-com", &[], vec![geo.key()]),
            false,
        );
        assert_eq!(p.conflicts, vec![("example.com.".to_string(), geo.key())]);
        assert!(p.zone_changes.is_empty(), "nothing may be planned for it");
        assert!(!p.is_empty(), "a conflict is not an empty plan");
    }

    // ---- determinism ----------------------------------------------------------------------------

    #[test]
    fn plan_is_deterministic() {
        let d = want(&[
            rr("a.", "A", 300, &["192.0.2.10"]),
            rr("b.", "A", 300, &["192.0.2.11"]),
            rr("c.", "TXT", 300, &["x"]),
        ]);
        let o = have(&[
            rr("b.", "A", 900, &["192.0.2.11"]),
            rr("z.", "TXT", 300, &["y"]),
        ]);
        let first = plan(&d, &o, false);
        for _ in 0..5 {
            assert_eq!(plan(&d, &o, false), first);
        }
    }

    // ---- SOA ------------------------------------------------------------------------------------

    fn soa(rrdatas: &[&str]) -> Rrset {
        Rrset {
            name: "example.com.".to_string(),
            rtype: "SOA".to_string(),
            ttl: 21600,
            rrdatas: rrdatas.iter().map(|s| s.to_string()).collect(),
        }
    }

    const SOA_RDATA: &str = "ns1.example.net. hostmaster.example.net. 3 21600 3600 259200 300";

    /// Cloud DNS leaves the serial alone on changes.create - verified against the live service -
    /// so the bump is ours to emit, in the same atomic change.
    #[test]
    fn a_changed_zone_carries_a_soa_bump() {
        let p = plan(
            &want(&[rr("www.", "A", 300, &["192.0.2.10"])]),
            &have(&[soa(&[SOA_RDATA])]),
            false,
        );
        let c = only_change(&p);
        let (old, new) = c.soa.as_ref().expect("a changed zone bumps its serial");
        assert_eq!(old.rrdatas[0].split_whitespace().nth(2), Some("3"));
        assert_eq!(new.rrdatas[0].split_whitespace().nth(2), Some("4"));
        assert!(!c.soa_bump_skipped);
    }

    /// Kept out of additions/deletions so bookkeeping never shows up in the reported counts.
    #[test]
    fn the_soa_pair_stays_out_of_additions_deletions_and_counts() {
        let p = plan(
            &want(&[rr("www.", "A", 300, &["192.0.2.10"])]),
            &have(&[soa(&[SOA_RDATA])]),
            false,
        );
        let c = only_change(&p);
        assert_eq!(c.additions.len(), 1, "only the real record");
        assert!(c.deletions.is_empty());
        assert!(c.updates.is_empty());
        let counts = p.counts();
        assert_eq!((counts.added, counts.updated, counts.removed), (1, 0, 0));
    }

    #[test]
    fn an_unchanged_zone_gets_no_soa_bump() {
        let rrsets = [rr("www.", "A", 300, &["192.0.2.10"])];
        let mut observed = rrsets.to_vec();
        observed.push(soa(&[SOA_RDATA]));
        assert!(plan(&want(&rrsets), &have(&observed), false).is_empty());
    }

    #[test]
    fn an_unparseable_soa_is_skipped_rather_than_fatal() {
        let p = plan(
            &want(&[rr("www.", "A", 300, &["192.0.2.10"])]),
            &have(&[soa(&["not a soa"])]),
            false,
        );
        let c = only_change(&p);
        assert!(c.soa.is_none());
        assert!(c.soa_bump_skipped);
        assert_eq!(c.additions.len(), 1, "the real change still goes through");
    }

    #[test]
    fn a_zone_with_no_observed_soa_is_skipped() {
        let p = plan(
            &want(&[rr("www.", "A", 300, &["192.0.2.10"])]),
            &have(&[]),
            false,
        );
        assert!(only_change(&p).soa_bump_skipped);
    }

    #[test]
    fn bump_soa_increments_field_two() {
        let bumped = bump_soa(&soa(&[SOA_RDATA])).expect("bumps");
        assert_eq!(
            bumped.rrdatas[0],
            "ns1.example.net. hostmaster.example.net. 4 21600 3600 259200 300"
        );
        assert_eq!(bumped.ttl, 21600, "everything else is untouched");
    }

    #[test]
    fn bump_soa_wraps_at_u32_max() {
        let rdata = format!(
            "ns1.example.net. hostmaster.example.net. {} 1 2 3 4",
            u32::MAX
        );
        let bumped = bump_soa(&soa(&[&rdata])).expect("bumps");
        assert!(
            bumped.rrdatas[0].contains(" 0 "),
            "serial must wrap, not saturate"
        );
    }

    #[test]
    fn bump_soa_rejects_wrong_field_count() {
        assert!(bump_soa(&soa(&["ns1.example.net. hostmaster.example.net. 3"])).is_none());
    }

    #[test]
    fn bump_soa_rejects_a_non_numeric_serial() {
        assert!(
            bump_soa(&soa(&[
                "ns1.example.net. hostmaster.example.net. x 21600 3600 259200 300"
            ]))
            .is_none()
        );
    }

    #[test]
    fn bump_soa_rejects_multiple_rrdatas() {
        assert!(bump_soa(&soa(&[SOA_RDATA, SOA_RDATA])).is_none());
        assert!(bump_soa(&soa(&[])).is_none());
    }
}
