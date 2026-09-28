# Changelog

All notable changes to this project will be documented in this file.

This project adheres to [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)
and follows [Semantic Versioning](https://semver.org/).

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
