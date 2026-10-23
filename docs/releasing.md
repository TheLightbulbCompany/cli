# Releasing

1. Set `version` in `Cargo.toml` (and run `cargo build` so `Cargo.lock` follows), merge to `main`.
2. Tag and push: `git tag v<x.y.z> && git push origin v<x.y.z>`.
3. `.github/workflows/release.yml` runs on the tag:
   - `macos-arm64` builds on Apple silicon and creates the GitHub release with
     `lightbulb-macos-arm64.tar.gz` (it holds one binary, `lightbulb`), its `.sha256` and
     `install.sh`. The curl installer
     always takes the latest release, so it is live as soon as this job ends.
   - `publish-npm` sets `npm/package.json` (package `lightbulb-cli`) to the tag's version and runs
     `npm publish --provenance --access public`. Needs the `NPM_TOKEN` secret.
   - `bump-brew` rewrites `url`, `sha256` and the version in the test line (`lightbulb <x.y.z>`)
     of `Formula/lightbulb.rb` in `TheLightbulbCompany/homebrew-tap`, commits `lightbulb <x.y.z>`
     and pushes. Needs the `HOMEBREW_TAP_TOKEN` secret.
4. A job whose secret is missing passes with `NPM_TOKEN not set; skipping npm publish` or
   `HOMEBREW_TAP_TOKEN not set; skipping brew bump` in its log. Then do it by hand:
   - npm: `cd npm && npm version <x.y.z> --no-git-tag-version --allow-same-version && npm publish --access public`
     (from a logged-in machine; no provenance outside CI).
   - brew: in a homebrew-tap clone, set `url` to the release's tarball URL, `sha256` to the
     first field of the release's `.sha256` asset and the test's version, then commit
     `lightbulb <x.y.z>` and push.

Secrets live in this repo's Settings → Secrets and variables → Actions. The npm package
downloads the binary from the release whose tag matches its own version, so publish npm only
after the release assets exist (the workflow's `needs:` does this).
