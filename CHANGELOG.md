# Changelog

All notable changes to this project will be documented in this file.

This project adheres to [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)
and follows [Semantic Versioning](https://semver.org/).

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
