# Releasing daychat

Clients only install updates whose `SHA256SUMS` carries a valid signature from the release key built into them (`packaging/release.pub`). The secret key never touches GitHub, so someone who takes over the GitHub account still can't push code to users.

## One-time setup (on the Arch desktop)

```sh
sudo pacman -S --needed minisign github-cli
gh auth login
minisign -G -p packaging/release.pub -s ~/.minisign/daychat.key
```

Commit `packaging/release.pub`. Keep `~/.minisign/daychat.key` and its password safe and backed up offline. Losing it means existing clients can never update again; leaking it means anyone with the GitHub account can push code to every user.

## Each release

1. Bump the version in `Cargo.toml`, `Cargo.lock` and `packaging/arch/PKGBUILD` (CI checks that they match).
2. Commit, then tag and push: `git tag -a v0.4.0 -m "daychat v0.4.0" && git push origin main v0.4.0`.
3. CI builds every platform and creates a **draft** release. Drafts are invisible to `/update`.
4. On the Arch desktop: `MINISIGN_KEY=~/.minisign/daychat.key scripts/sign-release.sh v0.4.0`. It shows the checksums, signs them with the trusted comment `daychat v0.4.0`, verifies the signature, uploads it and publishes the release.

Clients see the new version at their next start; joining servers is blocked until they run `/update`.

## Rotating the key

Ship a release that contains the new `release.pub`, signed with the **old** key, then sign later releases with the new one. Clients that skip that release can't update automatically and need a manual install.
