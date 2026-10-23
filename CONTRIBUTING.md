# Contributing

Before opening a pull request, run:

    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test

CI runs the same checks on macOS, and a secret scan (gitleaks) on Linux. Contributions
are accepted under Apache-2.0. Report vulnerabilities per SECURITY.md, not in issues.
