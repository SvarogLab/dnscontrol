use crate::cli::Cli;
use crate::gcp::Dns;
use crate::load::{self, ConfigFile};
use crate::model::Counts;
use crate::run::converge;
use anyhow::{Context, Result, bail};
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, new_debouncer};
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

/// Whether the config actually changed. Comparing the loaded text directly rather than hashing it:
/// the config is a few kilobytes, so this is exact and costs nothing.
///
/// Its job is to save round trips, not CPU — without it a spurious event (a `touch`, the old
/// timestamped directory being removed after a ConfigMap swap, an editor's swap file) would cost a
/// full `observe()`, which lists every zone and every record set in the project.
pub fn should_reconverge(last: Option<&Vec<ConfigFile>>, now: &Vec<ConfigFile>) -> bool {
    last != Some(now)
}

pub async fn run(cli: &Cli, dns: &Dns) -> Result<Counts> {
    let dir = cli.config_dir.as_path();
    // inotify cannot watch a path that does not exist, and a mistyped mount must be loud rather
    // than silently idle.
    let meta = std::fs::metadata(dir).with_context(|| format!("cannot watch {}", dir.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    if dir.join("..data").exists() {
        tracing::debug!("config directory looks like a Kubernetes ConfigMap mount");
    }

    let (wake_tx, mut wake_rx) = mpsc::channel::<()>(1);
    let (err_tx, mut err_rx) = mpsc::channel::<String>(1);

    let watched = dir.to_path_buf();
    let mut debouncer = new_debouncer(
        Duration::from_millis(cli.debounce_ms),
        None,
        move |result: DebounceEventResult| match result {
            // A full channel already means "a converge is pending", so dropping the send is the
            // coalescing, not a lost event.
            Ok(events) => {
                for event in &events {
                    tracing::debug!(kind = ?event.kind, paths = ?event.paths, "fs event");
                }
                // notify reports the watched directory being removed as an ordinary Remove event,
                // never as a watch error - so this has to be caught here or the process lives on
                // with an inotify watch bound to a dead inode, seeing nothing, forever. Recreating
                // the directory at the same path does not revive it either.
                if events
                    .iter()
                    .any(|e| matches!(e.kind, EventKind::Remove(_)) && e.paths.contains(&watched))
                {
                    let _ = err_tx.try_send(format!("{} was removed", watched.display()));
                    return;
                }
                let _ = wake_tx.try_send(());
            }
            Err(errors) => {
                let _ = err_tx.try_send(format!("{errors:?}"));
            }
        },
    )
    .context("failed to start the filesystem watcher")?;

    // Always non-recursive, and always on the directory rather than the files.
    //
    // A ConfigMap key is a symlink into `..data` whose target string never changes; inotify
    // dereferences it, binds to the doomed timestamped inode, and dies with IN_DELETE_SELF on the
    // first update. The same failure hits vim/VS Code/`mv` write-then-rename on a laptop. A
    // directory watch sees kubelet's `rename("..data_tmp", "..data")` as IN_MOVED_TO directly
    // inside the watched directory, and an editor's save as IN_MODIFY — one mechanism, both modes.
    //
    // Recursive would be worse: notify follows symlinks when recursing, so it would descend into
    // the not-yet-live timestamped directory and race its deletion.
    debouncer
        .watch(dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch {}", dir.display()))?;
    tracing::info!(dir = %dir.display(), debounce_ms = cli.debounce_ms, "watching");

    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to install a SIGTERM handler")?;
    let mut sigint =
        signal(SignalKind::interrupt()).context("failed to install a SIGINT handler")?;

    // What we are trying to converge, versus what last converged. They differ only while a retry
    // is outstanding, which is also what makes the content gate skip a no-op after a failure.
    let mut current = load::read_config_dir(dir).await?;
    let mut state = Converger::new(cli, dns);

    // Converge once before any event: a pod that starts must apply what is already on disk.
    state.attempt(&current, "startup").await?;

    loop {
        // `pending()` disables this branch entirely when no retry is outstanding. Keeping the sleep
        // inside the select is what keeps signals responsive while we are backing off.
        let backoff = async {
            match state.retry_in {
                Some(delay) => tokio::time::sleep(delay).await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received, shutting down");
                break;
            }
            _ = sigint.recv() => {
                tracing::info!("SIGINT received, shutting down");
                break;
            }
            Some(error) = err_rx.recv() => {
                // A dead watch is invisible: the process would keep running and converge nothing.
                // Exit non-zero instead and let the Deployment restart us with a fresh watch.
                bail!("filesystem watch failed: {error}");
            }
            _ = backoff => {
                state.attempt(&current, "retry").await?;
            }
            Some(()) = wake_rx.recv() => {
                let files = match load::read_config_dir(dir).await {
                    Ok(files) => files,
                    // Belt and braces for the same hazard: if the directory is still missing after
                    // read_config_dir's retries, the watch cannot come back on its own.
                    Err(e) if !dir.exists() => {
                        return Err(e).with_context(|| {
                            format!("{} is gone; the filesystem watch is dead", dir.display())
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = format!("{e:#}"), "could not read the config directory");
                        continue;
                    }
                };
                if !should_reconverge(state.applied.as_ref(), &files) {
                    tracing::debug!("config unchanged, skipping converge");
                    continue;
                }
                // A fresh edit supersedes whatever we were retrying, and starts the backoff over.
                current = files;
                state.retry_in = None;
                state.attempt(&current, "config changed").await?;
            }
        }
    }

    debouncer.stop();
    Ok(state.counts)
}

/// First and longest wait between retries of a converge Google could not serve.
const RETRY_INITIAL: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// The converge loop's mutable state: what last succeeded, and how long to wait before trying again.
struct Converger<'a> {
    cli: &'a Cli,
    dns: &'a Dns,
    counts: Counts,
    /// The files of the last successful converge — the content gate compares against this, so a
    /// failed attempt is retried rather than skipped as "unchanged".
    applied: Option<Vec<ConfigFile>>,
    retry_in: Option<Duration>,
}

impl<'a> Converger<'a> {
    fn new(cli: &'a Cli, dns: &'a Dns) -> Self {
        Self {
            cli,
            dns,
            counts: Counts::default(),
            applied: None,
            retry_in: None,
        }
    }

    /// Runs one converge, absorbing errors Google will probably not repeat.
    ///
    /// A 5xx is the service having a bad minute, not a broken configuration, so it is retried with
    /// backoff and the controller keeps watching. Everything else — a malformed document, a change
    /// Cloud DNS refused — is ours to fix and stays fail-fast, because a pod that crash-loops on a
    /// bad config push is the loud signal we want.
    ///
    /// This is not a second layer over the client's own retries. gax's `Aip194Strict` bails out on
    /// any non-idempotent call (`if !state.idempotent { Permanent }`), so every mutation — every
    /// `changes.create` — reaches us unretried, and even reads are only retried on 503.
    async fn attempt(&mut self, files: &[ConfigFile], trigger: &str) -> Result<()> {
        match converge(self.cli, self.dns, files).await {
            Ok(done) => {
                self.counts = done;
                log_converge(self.cli, &self.counts, trigger);
                self.applied = Some(files.to_vec());
                self.retry_in = None;
                Ok(())
            }
            Err(e) => match server_error_status(&e) {
                Some(status) => {
                    let delay = next_backoff(self.retry_in);
                    tracing::warn!(
                        status,
                        error = format!("{e:#}"),
                        retry_in_secs = delay.as_secs(),
                        "Cloud DNS returned a server error, retrying"
                    );
                    self.retry_in = Some(delay);
                    Ok(())
                }
                None => Err(e),
            },
        }
    }
}

/// Doubles the wait, starting at `RETRY_INITIAL` and never exceeding `RETRY_MAX`.
fn next_backoff(current: Option<Duration>) -> Duration {
    current.map_or(RETRY_INITIAL, |d| (d * 2).min(RETRY_MAX))
}

/// The 5xx status behind an error, if the failure came from Google's side.
fn server_error_status(error: &anyhow::Error) -> Option<u16> {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<google_cloud_gax::error::Error>())
        .find_map(|gax| gax.http_status_code())
        .filter(|status| (500..600).contains(status))
}

fn log_converge(cli: &Cli, counts: &Counts, trigger: &str) {
    tracing::info!(
        trigger,
        added = counts.added,
        updated = counts.updated,
        removed = counts.removed,
        zones_created = counts.zones_created,
        zones_deleted = counts.zones_deleted,
        check = cli.check,
        "converge complete"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(pairs: &[(&str, &str)]) -> Vec<ConfigFile> {
        pairs
            .iter()
            .map(|(n, c)| (n.to_string(), c.to_string()))
            .collect()
    }

    #[test]
    fn the_first_load_always_converges() {
        assert!(should_reconverge(None, &files(&[("a.yaml", "x")])));
    }

    #[test]
    fn identical_content_does_not_reconverge() {
        let now = files(&[("a.yaml", "x"), ("b.yaml", "y")]);
        assert!(!should_reconverge(Some(&now.clone()), &now));
    }

    #[test]
    fn changed_content_reconverges() {
        let last = files(&[("a.yaml", "x")]);
        assert!(should_reconverge(Some(&last), &files(&[("a.yaml", "y")])));
    }

    #[test]
    fn an_added_or_removed_file_reconverges() {
        let last = files(&[("a.yaml", "x")]);
        assert!(should_reconverge(
            Some(&last),
            &files(&[("a.yaml", "x"), ("b.yaml", "y")])
        ));
        assert!(should_reconverge(Some(&last), &files(&[])));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut delay = next_backoff(None);
        assert_eq!(delay, Duration::from_secs(1));
        for expected in [2, 4, 8, 16, 32, 60, 60, 60] {
            delay = next_backoff(Some(delay));
            assert_eq!(delay, Duration::from_secs(expected));
        }
    }

    fn gax(status: u16) -> anyhow::Error {
        anyhow::Error::new(google_cloud_gax::error::Error::http(
            status,
            http::HeaderMap::new(),
            bytes::Bytes::new(),
        ))
        .context("failed to submit change for zone example.com.")
    }

    #[test]
    fn a_server_error_is_retryable() {
        for status in [500, 502, 503, 504] {
            assert_eq!(
                server_error_status(&gax(status)),
                Some(status),
                "for {status}"
            );
        }
    }

    /// A rejected change is ours to fix - retrying it would loop forever on the same 4xx.
    #[test]
    fn a_client_error_is_not_retryable() {
        for status in [400, 403, 404, 409, 412] {
            assert_eq!(server_error_status(&gax(status)), None, "for {status}");
        }
    }

    #[test]
    fn a_configuration_error_is_not_retryable() {
        let e = anyhow::anyhow!("snippet cycle detected: a -> b -> a");
        assert_eq!(server_error_status(&e), None);
    }

    #[test]
    fn a_renamed_file_with_the_same_content_reconverges() {
        let last = files(&[("a.yaml", "x")]);
        assert!(should_reconverge(Some(&last), &files(&[("b.yaml", "x")])));
    }
}
