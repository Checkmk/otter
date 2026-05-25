# Releasing otter

This file documents the maintainer process for cutting a new `otter` release.
Users do not need to read this — they get updates via `otter update`.

## TL;DR

```bash
# 1. bump the version in the workspace root Cargo.toml
$EDITOR Cargo.toml                            # version = "0.X.Y"

# 2. commit, tag, push
git add Cargo.toml Cargo.lock
git commit -m "Release v0.X.Y"
git tag v0.X.Y
git push origin main v0.X.Y

# 3. wait for the `release` GitHub Action to finish, then optionally polish
#    the auto-generated release notes on the GitHub Releases page.
```

That's it. Users on prior versions will see `▲ v0.X.Y available` in their TUI
the next time their daemon starts.

## Platform coverage

The current release workflow only builds `x86_64-unknown-linux-gnu`. macOS,
Windows, and Linux ARM users continue to build from source until we extend
the matrix. When that happens, the only change required is adding entries
to `strategy.matrix` in `.github/workflows/release.yml` plus a matching
`target` constant in `crates/otter-cli/src/updater/mod.rs`.
