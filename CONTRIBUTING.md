# Contributing

Before opening a pull request, run:

    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test

CI runs the same checks plus a secret scan (gitleaks) on Linux and macOS. Contributions
are accepted under Apache-2.0. Report vulnerabilities per SECURITY.md, not in issues.
