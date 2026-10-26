# Releasing

1. Set `version` in `Cargo.toml` (and run `cargo build` so `Cargo.lock` follows), merge to `main`.
2. Tag and push: `git tag v<x.y.z> && git push origin v<x.y.z>`.
3. `.github/workflows/release.yml` runs on the tag:
   - `macos-arm64` builds on Apple silicon and creates the GitHub release with
     `lightbulb-macos-arm64.tar.gz` (it holds one binary, `lightbulb`), its `.sha256` and
     `install.sh`. The curl installer
     always takes the latest release, so it is live on macOS as soon as this job ends.
   - `linux-arm64` and `linux-x86_64` then build static musl binaries and upload
     `lightbulb-linux-arm64.tar.gz` and `lightbulb-linux-x86_64.tar.gz` (each with a `.sha256`) to
     that release; `install.sh` picks the asset by `uname`. Until both jobs end, a Linux install of
     `latest` fails with a 404 (curl names the missing asset). npm and Homebrew stay macOS-only.
   - `publish-npm` sets `npm/package.json` (package `lightbulb-cli`) to the tag's version and runs
     `npm publish --provenance --access public` through npm trusted publishing (the package on
     npmjs.com trusts this repository's `release.yml` over OIDC); no npm token exists.
   - `bump-brew` rewrites `url`, `sha256` and the version in the test line (`lightbulb <x.y.z>`)
     of `Formula/lightbulb.rb` in `TheLightbulbCompany/homebrew-tap`, commits `lightbulb <x.y.z>`
     and pushes. Needs the `HOMEBREW_TAP_TOKEN` secret.
4. Sign the checksums (Linux and WSL `lightbulb update` installs nothing without them; macOS
   checks the codesign team instead). On the machine that holds the offline `release.key`,
   once the Linux jobs have ended:

   ```sh
   TAG=v<x.y.z>
   gh release download "$TAG" -R TheLightbulbCompany/cli -p '*.sha256' -D sig && cd sig
   for f in *.sha256; do
     openssl pkeyutl -sign -rawin -inkey ../release.key -in "$f" -out "$f.sig"
   done
   gh release upload "$TAG" -R TheLightbulbCompany/cli --clobber *.sha256.sig
   ```

   Each `.sig` is the raw 64-byte Ed25519 signature over the `.sha256` file's exact bytes.
5. The brew job passes with `HOMEBREW_TAP_TOKEN not set; skipping brew bump` in its log when its
   secret is missing; the npm job fails if the trusted publisher is not configured. Then do it by hand:
   - npm: `cd npm && npm version <x.y.z> --no-git-tag-version --allow-same-version && npm publish --access public`
     (from a logged-in machine; no provenance outside CI).
   - brew: in a homebrew-tap clone, set `url` to the release's tarball URL, `sha256` to the
     first field of the release's `.sha256` asset and the test's version, then commit
     `lightbulb <x.y.z>` and push.

## The release key (once)

Generate it offline, on a machine that is not CI, and keep `release.key` there (never in this
repository or in Actions secrets):

```sh
openssl genpkey -algorithm ed25519 -out release.key
openssl pkey -in release.key -pubout -outform DER | tail -c 32 | xxd -i
```

Paste the 32 bytes the second line prints into `RELEASE_KEY` in `src/cli.rs` (it is all zeros
until then, which verifies nothing), merge, then tag. A release built before that cannot update
a Linux install.

Secrets live in this repo's Settings → Secrets and variables → Actions. The npm package
downloads the binary from the release whose tag matches its own version, so publish npm only
after the release assets exist (the workflow's `needs:` does this).

## Rollback

- curl: pin a tag with the install line:
  `curl -fsSL https://github.com/TheLightbulbCompany/cli/releases/latest/download/install.sh | LIGHTBULB_VERSION=v0.2.2 sh`.
  From 0.2.2 on, `lightbulb update --version v0.2.2` does the same.
- brew users: the formula tracks latest; there is no versioned formula.
- npm: `npm i -g lightbulb-cli@0.2.2`, then `lightbulb setup`.
