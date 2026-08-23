use crate::config::{DocRef, Document, parse_documents};
use crate::model::Desired;
use crate::normalize::build_desired;
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::time::Duration;

/// One config file: its directory-relative name and its contents.
pub type ConfigFile = (String, String);

const ENOENT_RETRIES: usize = 3;
const ENOENT_BACKOFF: Duration = Duration::from_millis(100);

/// Whether a directory entry is one of our config files.
///
/// Rejecting every dot-prefixed name is what makes a Kubernetes ConfigMap mount work: kubelet's
/// bookkeeping entries are `..data`, `..data_tmp` and `..2026_08_22_19_47_01.123456789`, and one
/// rule excludes all three along with editor dotfiles.
pub fn is_config_entry(name: &str) -> bool {
    !name.starts_with('.')
        && (name.ends_with(".yaml") || name.ends_with(".yml"))
        && name.len() > ".yml".len()
}

/// Reads every config file in `dir`, sorted by name.
///
/// Retries only on `NotFound`, and only for the directory read as a whole: that covers the instant
/// during a ConfigMap swap when a symlink's target has been unlinked but the new `..data` is not
/// yet in place, without papering over a mount that simply is not there.
pub async fn read_config_dir(dir: &Path) -> Result<Vec<ConfigFile>> {
    for attempt in 1..=ENOENT_RETRIES {
        match read_once(dir) {
            Ok(files) => return Ok(files),
            Err(e) if is_not_found(&e) && attempt < ENOENT_RETRIES => {
                tracing::debug!(attempt, dir = %dir.display(), "config directory not ready, retrying");
                tokio::time::sleep(ENOENT_BACKOFF).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("the loop either returns or exhausts its attempts with an error")
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .any(|io| io.kind() == std::io::ErrorKind::NotFound)
}

fn read_once(dir: &Path) -> Result<Vec<ConfigFile>> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?;

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read {}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_config_entry(&name) {
            continue;
        }
        let path = entry.path();
        // metadata() follows symlinks, which is the point: in a ConfigMap every visible config
        // file is a symlink into ..data.
        match std::fs::metadata(&path) {
            Ok(md) if md.is_file() => {}
            Ok(_) => continue,
            Err(e) => {
                return Err(anyhow::Error::new(e))
                    .with_context(|| format!("failed to stat {}", path.display()));
            }
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        files.push((name, content));
    }

    files.sort();
    Ok(files)
}

/// Parses and validates a whole config directory. Pure: everything it needs is already in memory,
/// which is what lets the cross-file snippet behaviour be tested without touching a filesystem.
pub fn build(files: &[ConfigFile], default_ttl: u32) -> Result<Desired> {
    let mut docs: Vec<(DocRef, Document)> = Vec::new();
    for (name, content) in files {
        docs.extend(parse_documents(name, content)?);
    }
    if docs.is_empty() {
        bail!("no zone or snippet documents found — is the config directory empty?");
    }
    build_desired(&docs, default_ttl)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::DEFAULT_TTL;

    fn files(pairs: &[(&str, &str)]) -> Vec<ConfigFile> {
        pairs
            .iter()
            .map(|(n, c)| (n.to_string(), c.to_string()))
            .collect()
    }

    /// The listing is a real ConfigMap mount plus the things a developer's directory grows.
    #[test]
    fn is_config_entry_skips_kubelet_bookkeeping_and_non_yaml() {
        let cases = [
            ("snippets.yaml", true),
            ("example.com.yaml", true),
            ("example.com.yml", true),
            ("..data", false),
            ("..data_tmp", false),
            ("..2026_08_22_19_47_01.123456789", false),
            (".hidden.yaml", false),
            ("README.md", false),
            ("subdir", false),
            ("notes.yaml.swp", false),
            (".yaml", false),
            ("", false),
        ];
        for (name, expected) in cases {
            assert_eq!(is_config_entry(name), expected, "for {name:?}");
        }
    }

    #[test]
    fn snippets_resolve_across_files() {
        let desired = build(
            &files(&[
                (
                    "snippets.yaml",
                    "kind: snippets\nsnippets:\n  mx:\n    - {type: mx, ttl: 300, target: [\"10 mx1.mail.example.net.\"]}\n",
                ),
                (
                    "example.com.yaml",
                    "kind: zone\nzone: example.com\nrecords:\n  - {use: mx}\n",
                ),
            ]),
            DEFAULT_TTL,
        )
        .expect("builds");
        assert_eq!(desired.zones["example.com."].rrsets.len(), 1);
    }

    /// The registry is built from every document before any reference is resolved, so a snippet
    /// may be defined in a file that sorts after the zone that uses it.
    #[test]
    fn a_snippet_may_be_defined_after_the_zone_that_uses_it() {
        let desired = build(
            &files(&[
                (
                    "aaa-zone.yaml",
                    "kind: zone\nzone: example.com\nrecords:\n  - {use: mx}\n",
                ),
                (
                    "zzz-snippets.yaml",
                    "kind: snippets\nsnippets:\n  mx:\n    - {type: mx, target: [\"10 mx1.mail.example.net.\"]}\n",
                ),
            ]),
            DEFAULT_TTL,
        )
        .expect("builds");
        assert_eq!(desired.zones["example.com."].rrsets.len(), 1);
    }

    #[test]
    fn multiple_zones_across_files_are_all_collected() {
        let desired = build(
            &files(&[
                ("a.yaml", "kind: zone\nzone: example.com\n"),
                ("b.yaml", "kind: zone\nzone: example.net\n"),
            ]),
            DEFAULT_TTL,
        )
        .expect("builds");
        assert_eq!(
            desired.zones.keys().collect::<Vec<_>>(),
            vec!["example.com.", "example.net."]
        );
    }

    #[test]
    fn the_same_zone_in_two_files_is_an_error() {
        let e = format!(
            "{:#}",
            build(
                &files(&[
                    ("a.yaml", "kind: zone\nzone: example.com\n"),
                    ("b.yaml", "kind: zone\nzone: example.com\n"),
                ]),
                DEFAULT_TTL,
            )
            .expect_err("should fail")
        );
        assert!(e.contains("declared twice"), "unexpected: {e}");
        assert!(
            e.contains("a.yaml") && e.contains("b.yaml"),
            "unexpected: {e}"
        );
    }

    #[test]
    fn an_empty_config_directory_is_an_error() {
        let e = format!("{:#}", build(&[], DEFAULT_TTL).expect_err("should fail"));
        assert!(
            e.contains("no zone or snippet documents"),
            "unexpected: {e}"
        );
    }

    #[test]
    fn a_parse_error_names_its_file() {
        let e = format!(
            "{:#}",
            build(
                &files(&[("broken.yaml", "kind: zone\n  bad: [\n")]),
                DEFAULT_TTL
            )
            .expect_err("should fail")
        );
        assert!(e.contains("broken.yaml"), "unexpected: {e}");
    }
}
