# dnscontrol

Converges Google Cloud DNS to the zones declared in a directory of YAML documents. Runs as a
one-shot CLI with `--check`/`--diff`, or as a controller that watches a mounted ConfigMap and
reconverges when it changes.

> **Inside a declared zone this tool is authoritative:** any record set the config does not mention
> is **deleted**, except what an `ignore` rule claims. Point it only at zones you own.
>
> Whole zones are a separate decision. A zone that exists in the project but is not declared here is
> left alone by default, because a config that failed to load a file looks exactly like a config
> that meant to retire every zone in it. `--delete-undeclared-zones` turns that into deletion — it
> is meant for a deliberate, supervised run, not for a controller's steady state.

## Usage

```bash
# See what would change. Read-only: no mutating API call is issued.
dnscontrol --config-dir ./zones --check --diff

# Apply.
dnscontrol --config-dir ./zones --diff

# Stay running and reconverge whenever the directory changes.
dnscontrol --config-dir /etc/dnscontrol --watch --diff
```

The configuration is validated before anything touches the network, so a typo fails in
milliseconds and needs no credentials at all.

### Options

| Flag | Environment | Default |
| --- | --- | --- |
| `--config-dir` | `DNSCONTROL_CONFIG_DIR` | `/etc/dnscontrol` |
| `--credentials` | `GOOGLE_APPLICATION_CREDENTIALS` | Application Default Credentials |
| `--project` | `GOOGLE_CLOUD_PROJECT` | from the key, then the metadata server |
| `--check` | — | off |
| `--diff` | — | off |
| `--watch` | `DNSCONTROL_WATCH` | off |
| `--debounce-ms` | `DNSCONTROL_DEBOUNCE_MS` | `500` |
| `--default-ttl` | `DNSCONTROL_DEFAULT_TTL` | `900` |
| `--delete-undeclared-zones` | `DNSCONTROL_DELETE_UNDECLARED_ZONES` | off |

## Configuration

Every file in the directory is a YAML stream. Each document declares its `kind`, and a single file
may hold as many documents of either kind as you like. See [`examples/`](examples/).

```yaml
kind: zone
zone: example.com
records:
  - use: mail-stack
  - name: www
    type: a
    target: [192.0.2.10]
```

- **`name`** is a relative label. Omit it entirely for the zone apex; `@` and `""` are errors, and
  so is an FQDN like `www.example.com.` — the tool tells you what to write instead.
- **`type`** is case-insensitive. The apex `SOA` and `NS` belong to Cloud DNS and cannot be
  declared.
- **`ttl`** defaults to `--default-ttl`.
- **`target`** is the list of rdata strings in presentation format, exactly as Cloud DNS stores
  them. Quoting inside TXT values is yours to get right.

One record is one record set: a `(name, type)` pair with N values, not N separate records.

### Snippets

Reusable record groups, referenced by name. The registry is global and is built from every file
before any reference is resolved, so a snippet may live in any file, in any order.

```yaml
kind: snippets
snippets:
  mail-mx:
    - {type: mx, ttl: 300, target: ["10 mx1.mail.example.net."]}
  mail-stack:
    - use: mail-mx          # snippets may use snippets
```

A `use:` splices the snippet's records into the list in place, so declaration order is preserved.
Cycles are detected and reported as a path (`a -> b -> a`). A snippet spliced in twice produces a
duplicate record set, which is an error rather than a silent overwrite.

YAML anchors and merge keys (`<<:`) are rejected: `use:` is the one extension mechanism.

### Coexisting with other writers

`_acme-challenge*` TXT record sets are **always** ignored — an ACME DNS-01 solver creates and
deletes them while a certificate is being issued, and deleting one mid-flight fails the challenge.
For anything else another system owns, list it:

```yaml
kind: zone
zone: example.net
ignore:
  - {name: "_dnsauth*", type: txt}
```

`name` is a glob where `*` also matches dots, so `_acme-challenge*` covers both
`_acme-challenge` and `_acme-challenge.sub`. Omitting `type` matches any type. Declaring a record
that matches one of your own ignore rules is an error, not a silent no-op.

Record sets carrying a Cloud DNS routing policy (geo/weighted) are also left alone: this format has
no way to express them, so converging them would mean deleting them.

## Credentials

Application Default Credentials, with nothing to configure. The credential kind is read from the
key's own `type` field, so a service account key, a `gcloud auth application-default login` user
credential, workload identity federation and the GKE/GCE metadata server all work through the same
path. `--credentials <key.json>` is just a friendlier spelling of
`GOOGLE_APPLICATION_CREDENTIALS`.

No OAuth scope is requested; each credential type uses its own default. Narrowing it to
`ndev.clouddns.readwrite` would break `gcloud auth application-default login` credentials outright,
since a refresh-token grant can only return scopes the user already consented to. Privileges come
from the IAM role on the identity (`roles/dns.admin`), not from the scope.

Cloud DNS needs an explicit project on every request and the auth stack does not expose one, so it
is resolved from `--project`, then `project_id` inside the key file, then the metadata server.

### TXT values

Cloud DNS stores TXT rdata in presentation format, i.e. **with** the surrounding quotes. A value
written as `"v=spf1 -all"` in a zone file is the six-character-plus-quotes string, so in YAML it
needs the quotes to survive:

```yaml
- type: txt
  target:
    - '"v=spf1 -all"'      # correct: the string contains quotes
    - "v=spf1 -all"        # wrong: YAML eats the quotes, and this will diff forever
```

The second form is not rejected — a TXT value is opaque text and the tool cannot know your intent —
but it will never converge, because Cloud DNS re-quotes what it stores and the two never match.

## Watch mode

`--watch` puts a non-recursive inotify watch on the **directory**, never on the individual files.
That is what makes one mechanism work in both places: a Kubernetes ConfigMap key is a symlink into
`..data` whose target string never changes, so a watch on the file binds to a doomed inode and dies
on the first update — and the same failure hits `vim`, `mv` and `git checkout` on a laptop.

Events are debounced, then the directory is re-read and compared; identical content skips the
converge entirely. There is no periodic resync: the tool reconverges on a config change and nothing
else, which is what lets an ACME solver own its own records in peace.

A converge in flight always finishes before `SIGTERM` is acted on.

Failures are split by who has to fix them. A 5xx from Google is retried with backoff (1s doubling to
60s) and logged as a warning — the controller keeps watching, and a fresh config edit supersedes
whatever was being retried. Anything else — a malformed document, a change Cloud DNS refused — exits
non-zero, because a pod that crash-loops on a bad config push is the signal you want. A failed
filesystem watch also exits: a watcher that has stopped seeing changes is worse than one that is
visibly gone.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Everything committed here — examples, test fixtures, sample output — uses only RFC 2606
(`example.com`) and RFC 5737 (`192.0.2.0/24`) reserved names. Real zone configuration and
credentials go in `data/`, which is gitignored.
