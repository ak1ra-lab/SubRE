# AGENTS.md

## Commands

| Step | Command |
|------|---------|
| Format (required before commit) | `cargo fmt` |
| Format check | `cargo fmt --check` |
| Lint (required before commit) | `cargo clippy --all-targets -- -D warnings` |
| Unit + corpus tests | `cargo test` |
| Lib-only fast iteration | `cargo test --lib` |
| Release binary (~20 MB stripped) | `cargo build --release` |

`cargo run` is GUI and needs a display — do not invoke from automated contexts.

## Lint / format config

Strict. Configured in `rustfmt.toml`, `clippy.toml`, and `Cargo.toml` `[lints]`. Allowed low-signal lints (package-level allow) are listed in `Cargo.toml`; new ones must come with a `#[allow(...)]` and a one-line reason. `unsafe_code = "forbid"`. `multiple_crate_versions` is allowed because eframe pulls many transitive deps that we don't pin.

## Module layout

- `src/core/` — GUI-free, unit-tested: `parse.rs`, `matcher.rs`, `plan.rs`, `execute.rs`, `history.rs`, `config.rs`. New behavior goes here first.
- `src/ui/` — `app.rs` (eframe::App), `dnd_drop.rs`, `fonts.rs` (CJK font registered at startup — preserve).
- `tests/corpus.rs` + `tests/fixtures/` — integration tests driven by JSON fixtures.
- `src/main.rs` is a thin eframe launcher; `src/lib.rs` is the public API.

## Code conventions

- `EpisodeKey` in `src/core/parse.rs` has a custom numeric-aware `Ord` — do **not** re-derive `PartialOrd`/`Ord` (see comment on the impl).
- Config source-of-truth is `UserConfig` in `src/core/config.rs`. The TOML layout under `[suffix]`, `custom_*_exts`, `video_regex`, `subtitle_regex`, `action_mode` is canonical.
- `ActionMode` (Auto/Copy/Move) is global per plan; per-op override is out of scope.
- CJK strings in spec / delta files are intentional; the CJK font is registered at GUI startup (`src/ui/fonts.rs`).

## Test conventions

- Unit tests live in `#[cfg(test)] mod tests` inside each `core/` file. Use a `tmp_db()` helper with a process-id+counter+thread-name path so parallel `cargo test` doesn't collide.
- Integration tests: `tests/corpus.rs` loads every `tests/fixtures/*.json` (case `wontfix: true` in the JSON is skipped with a reason). Add a fixture when adding new matcher behavior.
- Filesystem-touching tests use `std::env::temp_dir()` plus a unique subdir; clean up with `std::fs::remove_dir_all` at the end.

## OpenSpec workflow

`openspec/` is the source of truth for spec-driven change management. New work goes in `openspec/changes/<name>/` with `proposal.md` + `specs/**/*.md` + `design.md` + `tasks.md`. After implementation, archive to `openspec/changes/archive/YYYY-MM-DD-<name>/` via `openspec archive`. The CLI is `openspec` (already installed); schema is `spec-driven`. The `openspec` branch holds spec/change artifacts; the `dev` branch holds code; PRs flow `openspec` → `dev` after each change is implemented and archived.

## Note on README.md

`README.md` is partially stale (module tree missing `dnd_drop.rs`/`fonts.rs`, binary-size note predates the current deps, "Per design D2" references a doc that doesn't live in the repo). Treat the code as the source of truth; do not derive structure or behavior from `README.md` until it's refreshed.