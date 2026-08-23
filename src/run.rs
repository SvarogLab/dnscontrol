use crate::cli::Cli;
use crate::diff;
use crate::gcp::Dns;
use crate::load::{self, ConfigFile};
use crate::model::{Counts, Plan, ZoneChange};
use crate::plan::plan;
use crate::{gcp, watch};
use anyhow::{Context, Result};

pub async fn run(cli: &Cli) -> Result<Counts> {
    // Validate the configuration before touching the network. A typo in a zone file should fail in
    // milliseconds with a precise message, not after an authentication round trip - and it means
    // the whole config format can be exercised offline, with no credentials at all.
    let files = load::read_config_dir(&cli.config_dir).await?;
    let desired = load::build(&files, cli.default_ttl)?;
    tracing::info!(
        zones = desired.zones.len(),
        config_dir = %cli.config_dir.display(),
        "configuration is valid"
    );

    let credentials = gcp::credentials()?;
    let project = gcp::resolve_project(cli.project.as_deref(), cli.credentials.as_deref()).await?;
    tracing::info!(project, "connecting to Cloud DNS");

    let dns = Dns::connect(project, credentials).await?;

    if cli.watch {
        watch::run(cli, &dns).await
    } else {
        converge(cli, &dns, &files).await
    }
}

/// One full pass: desired state from the files, observed state from GCP, the diff between them,
/// and — unless this is a check run — the changes that close the gap.
pub async fn converge(cli: &Cli, dns: &Dns, files: &[ConfigFile]) -> Result<Counts> {
    let desired = load::build(files, cli.default_ttl)?;
    let observed = dns.observe().await?;
    let plan = plan(&desired, &observed, cli.delete_undeclared_zones);

    if cli.diff {
        diff::print(&diff::render(&plan));
    }

    // A declared record that GCP holds under a routing policy can never converge - Cloud DNS would
    // answer 409 on every run - so refuse the whole plan rather than apply the rest and leave a
    // permanent failure behind.
    if !plan.conflicts.is_empty() {
        let listed: Vec<String> = plan
            .conflicts
            .iter()
            .map(|(zone, (name, rtype))| format!("{name}/{rtype} in zone {zone}"))
            .collect();
        anyhow::bail!(
            "these record sets are declared in the config but GCP holds them under a routing \
             policy, which this tool cannot express: {}",
            listed.join(", ")
        );
    }

    if plan.is_empty() {
        tracing::debug!("already converged, nothing to do");
        return Ok(plan.counts());
    }

    // The single point where a check run stops. Everything above it is read-only.
    if !cli.check {
        apply(dns, &plan).await?;
    }

    Ok(plan.counts())
}

async fn apply(dns: &Dns, plan: &Plan) -> Result<()> {
    for zone in &plan.zones_to_create {
        tracing::info!(zone = %zone.dns_name, "creating managed zone");
        let resource_name = dns.create_zone(zone).await?;
        if zone.rrsets.is_empty() {
            continue;
        }
        // A fresh zone holds only the SOA and NS that Cloud DNS writes itself, both protected, so
        // every declared record is a pure addition.
        dns.apply_change(&ZoneChange {
            dns_name: zone.dns_name.clone(),
            resource_name,
            additions: zone.rrsets.values().cloned().collect(),
            deletions: Vec::new(),
            updates: Vec::new(),
            // A zone Cloud DNS just created carries a fresh serial; there is nothing to bump.
            soa: None,
            soa_bump_skipped: false,
        })
        .await
        .with_context(|| format!("failed to populate new zone {}", zone.dns_name))?;
    }

    for change in &plan.zone_changes {
        if change.soa_bump_skipped {
            tracing::warn!(
                zone = %change.dns_name,
                "apex SOA could not be parsed; applying the change without a serial bump"
            );
        }
        tracing::info!(
            zone = %change.dns_name,
            additions = change.additions.len(),
            deletions = change.deletions.len(),
            serial = change.soa.as_ref().map(|(_, new)| new.rrdatas[0].split_whitespace().nth(2).unwrap_or("?")),
            "applying change"
        );
        dns.apply_change(change).await?;
    }

    // Zone deletion goes last: it is the one irreversible step, and doing it after everything else
    // means a failure earlier in the run cannot leave a zone gone and its replacement unwritten.
    for zone in &plan.zones_to_delete {
        tracing::warn!(zone = %zone.dns_name, "deleting undeclared managed zone");
        dns.delete_zone(zone).await?;
    }

    Ok(())
}
