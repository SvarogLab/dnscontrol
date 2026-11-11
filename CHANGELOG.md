# Changelog

All notable changes to this project will be documented in this file.

This project adheres to [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)
and follows [Semantic Versioning](https://semver.org/).

## [0.4.1] - 2026-11-11

### Build system 🛠️

- Stop tracking the lockfile
- Track the lockfile and check it with cargo-deny (#19)

### CI/CD ⚙️

- Fail the image tag check when the chart renders no image
- Attach the release chart by its exact name

### Fixed 🐛

- Update rust crate google-cloud-dns-v1 to v2
- Update rust crate serde-saphyr to v2
- Update google-cloud-rust monorepo (#20)
- Update rust crate notify to 8.2.2 (#21)
- Update rust crate serde_json to 1.0.154 (#22)
- Update rust crate serde-saphyr to 2.1.1 (#23)
- Update rust crate tokio to 1.54.1 (#24)

### Miscellaneous 🧹

- Let renovate update dependencies through the shared preset
- Update actions/checkout action to v7
- Update docker/build-push-action action to v7
- Update docker/login-action action to v4
- Update docker/setup-buildx-action action to v4
- Update github artifact actions
- Update docker/setup-qemu-action action to v4

## [0.4.0] - 2026-09-28

### Added ✨

- [**breaking**] Leave resource requests and limits to the deployment

### CI/CD ⚙️

- Attach the helm chart to the github release

### Documentation 📚

- State that the published image and chart are public

## [0.3.0] - 2026-09-28

### Added ✨

- Ship the helm chart from this repository

## [0.2.1] - 2026-09-28

### Build system 🛠️

- Update dependencies to their latest compatible releases

### Fixed 🐛

- Update rustls to 0.23.45 for RUSTSEC-2026-0285
- Stop waking the loop on its own directory reads

### Miscellaneous 🧹

- Move markdownlint config to the skeleton .markdownlint-cli2.jsonc

### Tests ✅

- Drop the assertion naming the removed flag

## [0.2.0] - 2026-08-23

### Added ✨

- [**breaking**] Make zone deletion opt-in
- Add --skip-soa-bump

### CI/CD ⚙️

- Take the release notes from CHANGELOG.md

### Changed 🔧

- [**breaking**] Prefix environment variables with DNSCONTROL_

### Miscellaneous 🧹

- Drop the gitignored data directory
- Mark breaking changes in generated entries
- Bump version to 0.2.0

## [0.1.0] - 2026-08-23

### Added ✨

- Converge Google Cloud DNS from a directory of YAML

### Fixed 🐛

- Lowercase the GHCR image name
