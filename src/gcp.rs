use crate::model::{DesiredZone, Observed, ObservedZone, Rrset, ZoneChange, is_apex_owned};
use anyhow::{Context, Result, bail};
use google_cloud_auth::credentials::Credentials;
use google_cloud_dns_v1::client::{Changes, ManagedZones, ResourceRecordSets};
use google_cloud_dns_v1::model;
use google_cloud_gax::exponential_backoff::ExponentialBackoffBuilder;
use google_cloud_gax::retry_policy::{Aip194Strict, RetryPolicyExt as _};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const METADATA_PROJECT_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/project/project-id";

const CHANGE_POLL_INITIAL: Duration = Duration::from_millis(250);
const CHANGE_POLL_MAX: Duration = Duration::from_secs(5);
/// Deliberately below a pod's termination grace period, so a converge cannot outlive SIGTERM.
const CHANGE_POLL_DEADLINE: Duration = Duration::from_secs(45);

/// Application Default Credentials. The credential *kind* is read from the key's own `type` field,
/// so there is nothing to configure: a service account key, a `gcloud auth application-default
/// login` user credential, workload identity federation and the GKE/GCE metadata server all work
/// through the same path.
///
/// No scope is requested. Asking for `ndev.clouddns.readwrite` would seem like least privilege but
/// breaks user credentials outright — a refresh-token grant can only return scopes the user already
/// consented to, so `gcloud auth application-default login` credentials fail with `invalid_scope`.
/// It also buys nothing for a service account, whose privileges come from its IAM roles rather than
/// from the scope on a JWT it signs for itself. Every credential type therefore falls back to its
/// own default, which is `cloud-platform` for service accounts.
pub fn credentials() -> Result<Credentials> {
    google_cloud_auth::credentials::Builder::default()
        .build()
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context(
            "could not obtain Google credentials; pass --credentials <key.json>, or run where \
             Application Default Credentials are available",
        )
}

/// Cloud DNS requires an explicit project on every request and nothing in the auth stack exposes
/// one, so it is resolved here: the flag, then the key file, then the metadata server.
pub async fn resolve_project(
    explicit: Option<&str>,
    key_path: Option<&std::path::Path>,
) -> Result<String> {
    if let Some(project) = explicit {
        return Ok(project.to_string());
    }
    if let Some(path) = key_path
        && let Some(project) = project_from_key(path)?
    {
        return Ok(project);
    }
    if let Some(project) = project_from_metadata().await {
        return Ok(project);
    }
    bail!(
        "could not determine the GCP project: pass --project, set GOOGLE_CLOUD_PROJECT, use a \
         service account key that carries \"project_id\", or run on GCE/GKE"
    )
}

fn project_from_key(path: &std::path::Path) -> Result<Option<String>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read credentials {}", path.display()))?;
    let key: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("credentials {} are not valid JSON", path.display()))?;
    Ok(key
        .get("project_id")
        .and_then(|v| v.as_str())
        .map(str::to_string))
}

async fn project_from_metadata() -> Option<String> {
    let response = reqwest::Client::new()
        .get(METADATA_PROJECT_URL)
        .header("Metadata-Flavor", "Google")
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let project = response.text().await.ok()?.trim().to_string();
    (!project.is_empty()).then_some(project)
}

pub struct Dns {
    project: String,
    zones: ManagedZones,
    rrsets: ResourceRecordSets,
    changes: Changes,
}

impl Dns {
    pub async fn connect(project: String, creds: Credentials) -> Result<Self> {
        // Aip194Strict rather than AlwaysRetry: it retries only what AIP-194 declares safe, which
        // matters when most of these calls mutate.
        let retry = || {
            Aip194Strict
                .with_time_limit(Duration::from_secs(30))
                .with_attempt_limit(5)
        };
        let backoff = || {
            ExponentialBackoffBuilder::new()
                .with_initial_delay(CHANGE_POLL_INITIAL)
                .with_maximum_delay(CHANGE_POLL_MAX)
                .build()
        };

        Ok(Self {
            zones: ManagedZones::builder()
                .with_credentials(creds.clone())
                .with_retry_policy(retry())
                .with_backoff_policy(backoff()?)
                .build()
                .await?,
            rrsets: ResourceRecordSets::builder()
                .with_credentials(creds.clone())
                .with_retry_policy(retry())
                .with_backoff_policy(backoff()?)
                .build()
                .await?,
            changes: Changes::builder()
                .with_credentials(creds)
                .with_retry_policy(retry())
                .with_backoff_policy(backoff()?)
                .build()
                .await?,
            project,
        })
    }

    pub async fn observe(&self) -> Result<Observed> {
        let mut observed = Observed::default();

        for zone in self.list_zones().await? {
            let resource_name = zone.name.clone().context("managed zone has no name")?;
            let dns_name = zone
                .dns_name
                .clone()
                .context("managed zone has no dnsName")?;

            let mut rrsets = BTreeMap::new();
            let mut unsupported = Vec::new();
            for api in self.list_rrsets(&resource_name).await? {
                match from_api_rrset(&api)? {
                    Some(rrset) => {
                        rrsets.insert(rrset.key(), rrset);
                    }
                    None => {
                        let key = (
                            api.name.clone().unwrap_or_default(),
                            api.r#type.clone().unwrap_or_default(),
                        );
                        tracing::warn!(
                            zone = %dns_name,
                            name = %key.0,
                            rtype = %key.1,
                            "record set uses a routing policy this tool cannot express; leaving it alone"
                        );
                        unsupported.push(key);
                    }
                }
            }

            // Split horizon: Cloud DNS allows a public and a private zone with the same dnsName.
            // Keying by dnsName alone would silently drop one of them, and with pruning on, the
            // survivor could be deleted in the other's name. Refuse rather than guess.
            if let Some(first) = observed.zones.get(&dns_name) {
                bail!(
                    "project holds two managed zones for {dns_name}: \"{}\" and \"{}\". This tool \
                     identifies zones by their DNS name, so it cannot tell split-horizon zones apart",
                    first.resource_name,
                    resource_name
                );
            }
            observed.zones.insert(
                dns_name.clone(),
                ObservedZone {
                    dns_name,
                    resource_name,
                    description: zone.description.clone().unwrap_or_default(),
                    rrsets,
                    unsupported,
                },
            );
        }

        Ok(observed)
    }

    // Pagination is threaded by hand rather than through the crate's `by_item()` paginator, which
    // seeds the first request with `pageToken=""`. Cloud DNS answers an empty pageToken on
    // .../rrsets with HTTP 503 "Backend Error" (managedZones tolerates it), so the paginator cannot
    // list records at all. Sending the parameter only when there actually is a token avoids it.
    async fn list_zones(&self) -> Result<Vec<model::ManagedZone>> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut request = self.zones.list().set_project(self.project.as_str());
            if let Some(token) = &page_token {
                request = request.set_page_token(token.as_str());
            }
            let page = request
                .send()
                .await
                .context("failed to list managed zones")?;
            out.extend(page.managed_zones);
            match page.next_page_token {
                Some(token) if !token.is_empty() => page_token = Some(token),
                _ => return Ok(out),
            }
        }
    }

    async fn list_rrsets(&self, resource_name: &str) -> Result<Vec<model::ResourceRecordSet>> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut request = self
                .rrsets
                .list()
                .set_project(self.project.as_str())
                .set_managed_zone(resource_name);
            if let Some(token) = &page_token {
                request = request.set_page_token(token.as_str());
            }
            let page = request
                .send()
                .await
                .with_context(|| format!("failed to list record sets in zone {resource_name}"))?;
            out.extend(page.rrsets);
            match page.next_page_token {
                Some(token) if !token.is_empty() => page_token = Some(token),
                _ => return Ok(out),
            }
        }
    }

    pub async fn create_zone(&self, zone: &DesiredZone) -> Result<String> {
        // `visibility` is deliberately left unset, which means public - setting it would fight
        // anyone who deliberately made a zone private.
        let body = model::ManagedZone::new()
            .set_name(zone.resource_name.as_str())
            .set_dns_name(zone.dns_name.as_str())
            .set_description(zone.description.as_str());

        let created = self
            .zones
            .create()
            .set_project(self.project.as_str())
            .set_body(body)
            .send()
            .await
            .with_context(|| format!("failed to create managed zone {}", zone.resource_name))?;

        created.name.context("created managed zone has no name")
    }

    /// Empties a zone, then deletes it.
    ///
    /// Cloud DNS refuses to delete a zone that still holds records (HTTP 400, `containerNotEmpty`),
    /// and equally refuses to let the apex SOA/NS go on their own. So everything else comes out in
    /// one atomic change first, and the zone delete takes those last two with it.
    pub async fn delete_zone(&self, zone: &ObservedZone) -> Result<()> {
        // A routing policy has no representation in our model, so we cannot hand it back to Cloud
        // DNS in a deletion and cannot empty the zone. Say so plainly instead of failing on a
        // confusing containerNotEmpty three calls later.
        if !zone.unsupported.is_empty() {
            let (name, rtype) = &zone.unsupported[0];
            bail!(
                "cannot delete zone {} because {name}/{rtype} uses a routing policy this tool \
                 cannot express; remove it by hand, or keep the zone declared",
                zone.dns_name
            );
        }

        let deletions: Vec<Rrset> = zone
            .rrsets
            .values()
            .filter(|rrset| !is_apex_owned(rrset, &zone.dns_name))
            .cloned()
            .collect();

        if !deletions.is_empty() {
            tracing::info!(
                zone = %zone.dns_name,
                records = deletions.len(),
                "emptying managed zone before deleting it"
            );
            self.apply_change(&ZoneChange {
                dns_name: zone.dns_name.clone(),
                resource_name: zone.resource_name.clone(),
                additions: Vec::new(),
                deletions,
                updates: Vec::new(),
                // Bumping the serial of a zone that is about to cease existing is pointless.
                soa: None,
                soa_bump_skipped: false,
            })
            .await
            .with_context(|| format!("failed to empty zone {} before deletion", zone.dns_name))?;
        }

        self.remove_zone(&zone.resource_name).await
    }

    async fn remove_zone(&self, resource_name: &str) -> Result<()> {
        match self
            .zones
            .delete()
            .set_project(self.project.as_str())
            .set_managed_zone(resource_name)
            .send()
            .await
        {
            Ok(()) => Ok(()),
            // Converging to "absent" succeeded; treating a concurrent delete as an error would
            // make the tool non-idempotent.
            Err(e) if e.http_status_code() == Some(404) => {
                tracing::warn!(zone = resource_name, "managed zone already absent");
                Ok(())
            }
            Err(e) => {
                Err(e).with_context(|| format!("failed to delete managed zone {resource_name}"))
            }
        }
    }

    /// Submits every addition and deletion for one zone as a single atomic change.
    pub async fn apply_change(&self, change: &ZoneChange) -> Result<()> {
        // The SOA pair is carried outside additions/deletions so bookkeeping never reaches the diff
        // or the counts; it joins the wire body here, in the same atomic change as the records it
        // describes.
        let mut additions: Vec<_> = change.additions.iter().map(to_api_rrset).collect();
        let mut deletions: Vec<_> = change.deletions.iter().map(to_api_rrset).collect();
        if let Some((old, new)) = &change.soa {
            additions.push(to_api_rrset(new));
            deletions.push(to_api_rrset(old));
        }
        let body = model::Change::new()
            .set_additions(additions)
            .set_deletions(deletions);

        let submitted = self
            .changes
            .create()
            .set_project(self.project.as_str())
            .set_managed_zone(change.resource_name.as_str())
            .set_body(body)
            .send()
            .await
            .with_context(|| {
                format!("failed to submit change for zone {}", change.resource_name)
            })?;

        self.await_change(&change.resource_name, submitted).await
    }

    async fn await_change(&self, resource_name: &str, mut change: model::Change) -> Result<()> {
        let id = change.id.clone().context("change has no id")?;
        let deadline = Instant::now() + CHANGE_POLL_DEADLINE;
        let mut delay = CHANGE_POLL_INITIAL;

        while change.status == Some(model::change::Status::Pending) {
            if Instant::now() >= deadline {
                bail!(
                    "change {id} on zone {resource_name} was still pending after {:?}",
                    CHANGE_POLL_DEADLINE
                );
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(CHANGE_POLL_MAX);
            change = self
                .changes
                .get()
                .set_project(self.project.as_str())
                .set_managed_zone(resource_name)
                .set_change_id(id.as_str())
                .send()
                .await
                .with_context(|| format!("failed to poll change {id} on zone {resource_name}"))?;
        }
        Ok(())
    }
}

pub fn to_api_rrset(rrset: &Rrset) -> model::ResourceRecordSet {
    model::ResourceRecordSet::new()
        .set_name(rrset.name.as_str())
        .set_type(rrset.rtype.as_str())
        .set_ttl(i32::try_from(rrset.ttl).unwrap_or(i32::MAX))
        .set_rrdatas(rrset.rrdatas.clone())
}

/// `Ok(None)` for a record set carrying a routing policy: those have no rrdatas and no
/// representation in this config format, so the caller records them as untouchable rather than
/// converging them into oblivion.
pub fn from_api_rrset(api: &model::ResourceRecordSet) -> Result<Option<Rrset>> {
    if api.routing_policy.is_some() {
        return Ok(None);
    }
    let name = api.name.clone().context("record set has no name")?;
    let rtype = api.r#type.clone().context("record set has no type")?;
    let ttl = api.ttl.context("record set has no ttl")?;
    Ok(Some(Rrset {
        name,
        rtype,
        ttl: u32::try_from(ttl).with_context(|| {
            format!(
                "record set {name_ref} has a negative ttl {ttl}",
                name_ref = api.name.as_deref().unwrap_or("?")
            )
        })?,
        rrdatas: api.rrdatas.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rrset() -> Rrset {
        Rrset {
            name: "www.example.com.".to_string(),
            rtype: "A".to_string(),
            ttl: 300,
            rrdatas: vec!["192.0.2.10".to_string()],
        }
    }

    #[test]
    fn to_api_rrset_maps_every_field() {
        let api = to_api_rrset(&rrset());
        assert_eq!(api.name.as_deref(), Some("www.example.com."));
        assert_eq!(api.r#type.as_deref(), Some("A"));
        assert_eq!(api.ttl, Some(300));
        assert_eq!(api.rrdatas, vec!["192.0.2.10".to_string()]);
    }

    #[test]
    fn conversion_round_trips() {
        let original = rrset();
        let back = from_api_rrset(&to_api_rrset(&original))
            .expect("converts")
            .expect("not a routing policy");
        assert_eq!(back, original);
    }

    #[test]
    fn multi_rrdata_round_trips_in_order() {
        let original = Rrset {
            name: "example.com.".to_string(),
            rtype: "MX".to_string(),
            ttl: 300,
            rrdatas: vec![
                "10 mx1.mail.example.net.".into(),
                "20 mx2.mail.example.net.".into(),
            ],
        };
        let back = from_api_rrset(&to_api_rrset(&original)).unwrap().unwrap();
        assert_eq!(back.rrdatas, original.rrdatas);
    }

    #[test]
    fn a_routing_policy_rrset_is_reported_as_unsupported() {
        let api = model::ResourceRecordSet::new()
            .set_name("geo.example.com.")
            .set_type("A")
            .set_ttl(300)
            .set_routing_policy(model::RRSetRoutingPolicy::new());
        assert!(from_api_rrset(&api).expect("converts").is_none());
    }

    #[test]
    fn an_rrset_without_a_name_is_an_error() {
        let api = model::ResourceRecordSet::new().set_type("A").set_ttl(300);
        assert!(from_api_rrset(&api).is_err());
    }

    #[test]
    fn an_rrset_without_a_ttl_is_an_error() {
        let api = model::ResourceRecordSet::new()
            .set_name("www.example.com.")
            .set_type("A");
        assert!(from_api_rrset(&api).is_err());
    }

    #[tokio::test]
    async fn an_explicit_project_wins() {
        let project = resolve_project(Some("chosen"), None)
            .await
            .expect("resolves");
        assert_eq!(project, "chosen");
    }

    #[tokio::test]
    async fn the_project_comes_from_the_key_file() {
        let dir = std::env::temp_dir().join(format!("dnscontrol-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("key.json");
        std::fs::write(
            &path,
            r#"{"type":"service_account","project_id":"from-key"}"#,
        )
        .expect("write");

        let project = resolve_project(None, Some(&path)).await.expect("resolves");
        assert_eq!(project, "from-key");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_malformed_key_file_is_an_error() {
        let dir = std::env::temp_dir().join(format!("dnscontrol-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("key.json");
        std::fs::write(&path, "not json").expect("write");

        let e = format!(
            "{:#}",
            resolve_project(None, Some(&path)).await.expect_err("fails")
        );
        assert!(e.contains("not valid JSON"), "unexpected: {e}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
