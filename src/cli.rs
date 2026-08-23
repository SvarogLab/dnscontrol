use clap::Parser;
use std::path::PathBuf;

/// Converges Google Cloud DNS to the zones declared in a directory of YAML documents.
#[derive(Parser, Debug)]
#[command(
    name = "dnscontrol",
    version,
    about = "Converges Google Cloud DNS managed zones and record sets to a directory of declarative YAML"
)]
pub struct Cli {
    /// Directory of `kind: zone` / `kind: snippets` YAML documents. In Kubernetes this is the
    /// ConfigMap mount point; dot-prefixed entries (`..data`, `..2026_…`) are ignored.
    #[arg(long, env = "DNSCONTROL_CONFIG_DIR", default_value = "/etc/dnscontrol")]
    pub config_dir: PathBuf,

    /// Service account JSON key. Omit to use Application Default Credentials, which also covers
    /// `gcloud auth application-default login` and the GKE/GCE metadata server.
    #[arg(long, env = "GOOGLE_APPLICATION_CREDENTIALS")]
    pub credentials: Option<PathBuf>,

    /// GCP project. Omit to take it from the credentials key, then the metadata server.
    #[arg(long, env = "GOOGLE_CLOUD_PROJECT")]
    pub project: Option<String>,

    /// Compute and report the plan without issuing any mutating API call.
    #[arg(long)]
    pub check: bool,

    /// Print the plan to stdout. Independent of --check.
    #[arg(long)]
    pub diff: bool,

    /// Stay running, watching --config-dir and reconverging on every real change.
    // num_args/default_missing_value rather than a bare SetTrue flag: clap runs the value parser
    // over env values too, so DNSCONTROL_WATCH=false would otherwise mean "true".
    #[arg(
        long,
        env = "DNSCONTROL_WATCH",
        num_args = 0..=1,
        default_missing_value = "true",
        default_value_t = false,
        action = clap::ArgAction::Set,
    )]
    pub watch: bool,

    /// Debounce window in milliseconds for filesystem events. One ConfigMap update produces a
    /// burst of them.
    #[arg(long, env = "DNSCONTROL_DEBOUNCE_MS", default_value_t = 500)]
    pub debounce_ms: u64,

    /// TTL applied to a record that declares none.
    #[arg(long, env = "DNSCONTROL_DEFAULT_TTL", default_value_t = crate::normalize::DEFAULT_TTL)]
    pub default_ttl: u32,

    /// Delete managed zones that exist in GCP but are not declared here. Off by default: dropping a
    /// zone is the one irreversible thing this tool can do, and a config that fails to load a file
    /// looks exactly like a config that meant to retire every zone in it.
    #[arg(
        long,
        env = "DNSCONTROL_DELETE_UNDECLARED_ZONES",
        num_args = 0..=1,
        default_missing_value = "true",
        default_value_t = false,
        action = clap::ArgAction::Set,
    )]
    pub delete_undeclared_zones: bool,

    /// Leave the apex SOA serial alone on zones that change. Cloud DNS never bumps it itself, so
    /// this means the serial stops reflecting the zone's contents.
    #[arg(
        long,
        env = "DNSCONTROL_SKIP_SOA_BUMP",
        num_args = 0..=1,
        default_missing_value = "true",
        default_value_t = false,
        action = clap::ArgAction::Set,
    )]
    pub skip_soa_bump: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("dnscontrol").chain(args.iter().copied()))
            .expect("parses")
    }

    #[test]
    fn defaults_are_one_shot_and_etc_dnscontrol() {
        let cli = parse(&[]);
        assert_eq!(cli.config_dir, PathBuf::from("/etc/dnscontrol"));
        assert!(!cli.watch);
        assert!(!cli.check);
        assert!(!cli.diff);
        assert!(!cli.delete_undeclared_zones);
        assert!(!cli.skip_soa_bump);
        assert_eq!(cli.debounce_ms, 500);
        assert_eq!(cli.default_ttl, 900);
        assert_eq!(cli.project, None);
        assert_eq!(cli.credentials, None);
    }

    #[test]
    fn watch_flag_without_a_value_is_true() {
        assert!(parse(&["--watch"]).watch);
    }

    #[test]
    fn watch_equals_false_is_false() {
        assert!(!parse(&["--watch=false"]).watch);
    }

    #[test]
    fn delete_undeclared_zones_equals_false_is_false() {
        assert!(!parse(&["--delete-undeclared-zones=false"]).delete_undeclared_zones);
    }

    #[test]
    fn delete_undeclared_zones_without_a_value_is_true() {
        assert!(parse(&["--delete-undeclared-zones"]).delete_undeclared_zones);
    }

    /// The old spelling was the inverse. Leaving it parseable would silently flip the meaning of an
    /// existing deployment's args, so it has to be rejected outright.
    #[test]
    fn the_old_keep_undeclared_zones_flag_is_gone() {
        assert!(Cli::try_parse_from(["dnscontrol", "--keep-undeclared-zones"]).is_err());
    }

    #[test]
    fn skip_soa_bump_without_a_value_is_true() {
        assert!(parse(&["--skip-soa-bump"]).skip_soa_bump);
    }

    #[test]
    fn skip_soa_bump_equals_false_is_false() {
        assert!(!parse(&["--skip-soa-bump=false"]).skip_soa_bump);
    }

    #[test]
    fn check_and_diff_are_independent() {
        let cli = parse(&["--check"]);
        assert!(cli.check && !cli.diff);
        let cli = parse(&["--diff"]);
        assert!(!cli.check && cli.diff);
    }

    #[test]
    fn unknown_flag_is_an_error() {
        assert!(Cli::try_parse_from(["dnscontrol", "--bogus"]).is_err());
    }
}
