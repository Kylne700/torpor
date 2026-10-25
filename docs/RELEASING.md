# Releasing Torpor

A release is a tag `vX.Y.Z` on a commit whose CI run is green. The release workflow (`.github/workflows/release.yml`) then builds the four binaries, attests them, and commits them to `main` under `dist/<os>-<arch>/torpor` (`dist/` is in `.gitignore`; only the workflow adds it, with `git add -f`). It stops if `main` moved after the tag.

1. Set the new version in these six places. `tests/launcher.rs` checks that `bin/torpor`, `.claude-plugin/plugin.json` and `Cargo.toml` agree, and `--locked` builds fail if `Cargo.lock` lags.
   - `Cargo.toml` (`version`) and `Cargo.lock` (the `torpor` package)
   - `bin/torpor` (`V=`)
   - `.claude-plugin/plugin.json` (`version`)
   - `src/destructive.rs` and `tests/a10_block.rs` (test strings that name `bin/X.Y.Z/torpor`)
2. Run `cargo test`, commit, push to `main`, and wait for CI to pass.
3. Tag that commit `vX.Y.Z` and push the tag. Push nothing else until the release run has finished.
4. When the bot's `dist` commit is on `main`, check every binary: `dist/linux-x64/torpor --version` prints `torpor X.Y.Z`, and each macOS and arm64 file contains the text `X.Y.Z` once (`grep -a -c -F X.Y.Z <file>` prints `1`; keep `-F`, because without it the dots match any byte and the count can be higher). The launcher installs a binary only if it reports exactly `torpor X.Y.Z`.

Until step 4 is done, `main` has the new `V=` and the old binaries, so the launcher runs the old binary from `dist/` in place and installs nothing (`demo/run.sh` without a local build does the same).
