# Releasing

Pushing a tag `vX.Y.Z` runs `.github/workflows/release.yml`, which checks the tag against the
`Cargo.toml` version, runs the CI gates, builds binaries for macOS and Linux on arm64 and x86-64,
and publishes a GitHub Release with one archive per target, a `SHA256SUMS` file and generated notes.

## Cutting a release

1. On a branch, set `version` in `Cargo.toml` to `X.Y.Z` and run `cargo check`, which updates the
   `babysit` entry in `Cargo.lock`. The release builds with `--locked`, so commit both files.
2. Open a PR titled `chore: release X.Y.Z` and merge it.
3. Tag the merge commit on `main` and push the tag:

   ```sh
   git switch main && git pull --ff-only
   git tag -s vX.Y.Z -m vX.Y.Z
   git push origin vX.Y.Z
   ```

4. Watch the run with `gh run watch`.

## When the run fails

A failed run leaves no public release, at most a draft that the next run replaces. Fix the cause on
`main`, then move the tag to the fixed commit:

```sh
git push --delete origin vX.Y.Z
git tag -d vX.Y.Z
```

and tag again from step 3. A published release is never replaced: to ship a fix after one, bump the
version.
