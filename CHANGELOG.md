# Changelog

All notable changes to ErmyaGraph Community are recorded here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.13.1] - 2026-08-23

### Changed

- Completed the rebrand from Tessera to ErmyaGraph across code, packages,
  documentation, automation and published metadata.

## [0.13.0] - 2026-08-09

### Changed

- Split licensing by deliverable: the embeddable core and Python bindings use
  MIT; the server-side Community components use BSL 1.1 and convert to
  Apache-2.0 after four years.
- Updated the minimum supported Rust version to 1.88.
- Migrated the Python bindings to PyO3 0.29.
- Replaced absolute-duration performance assertions with relative baselines.

### Security

- Updated vulnerable transitive dependencies reported by `cargo audit`.
- Replaced the deprecated `rustls-pemfile` parser with rustls PKI types.

### Operations

- Added Rust, Python, security-audit and container CI gates.
- Added reproducible Python wheel builds and tagged GitHub release automation.

[Unreleased]: https://github.com/mojobytes/ermya-graph/compare/v0.13.1...HEAD
[0.13.1]: https://github.com/mojobytes/ermya-graph/compare/v0.13.0...v0.13.1
[0.13.0]: https://github.com/mojobytes/ermya-graph/releases/tag/v0.13.0
