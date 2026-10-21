---
name: release
description: Cut a new xarness release — bump the version, tag vX.Y.Z and push; CI builds and publishes the binaries. Use when asked to release, publish or ship a version.
---

# Release

Argument (optional): `patch`, `minor`, `major` or an explicit `X.Y.Z`.

Everything goes through the `run` tool. If it isn't available (Plan mode), stop right away and ask to switch to Build — don't read `.git` or other files instead.

1. Gather the state in one call:
   ```sh
   git fetch -q && git status -sb --porcelain && tag=$(git describe --tags --abbrev=0) && echo "last: $tag" && git log --oneline "$tag"..HEAD
   ```
   Stop and report if: not on `main`, the tree is dirty, the branch is ahead/behind, or there are no commits since the tag.

2. Version: from the argument, otherwise `feat:` among the commits → minor, else patch. Show the version and the commits, ask "Release vX.Y.Z?" and end your turn — always, even if the argument was given. Continue only after the user explicitly says yes; anything else means stop.

3. Bump the `[package]` `version` in `Cargo.toml`, then in one call:
   ```sh
   cargo test -q && git commit -qam "chore: release vX.Y.Z" && git tag vX.Y.Z && git push -q && git push -q origin vX.Y.Z
   ```
   `cargo test` builds too, so `Cargo.lock` picks up the new version before the commit.

4. The tag triggers `.github/workflows/release.yml` (3 targets, ~5 min). Wait for it in one background call:
   ```sh
   sleep 5 && gh run watch "$(gh run list --workflow release.yml --limit 1 --json databaseId -q '.[0].databaseId')" --exit-status >/dev/null && gh release view vX.Y.Z --json url,assets -q '.url, (.assets | length)'
   ```
   Expect 6 assets. On failure show `gh run view <id> --log-failed`; never delete or move the tag without asking.

Report the version and the release URL — one line.
