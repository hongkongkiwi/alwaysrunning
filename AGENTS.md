# AGENTS

This repo is a tiny, opinionated process supervisor for long‑running binaries.
Keep changes minimal, predictable, and CLI‑first.

## Goals
- The CLI should be simple: one intent → one command.
- Behavior should be obvious and safe by default.
- Prefer stability over features.
- Keep the runtime small and fast.

## Non‑goals
- No multi‑VM orchestration.
- No system‑wide (multi‑user) service management.

## Commands
- Build: `cargo build --release`
- Test: `cargo test`

## Architecture
- Single binary: `runner` (see `src/main.rs`)
- State files live in `~/.alwaysrunning/`
- Supervisor loop is in `daemon` mode
- Logs are per‑instance files

## CLI design
- Favor subcommands over flags.
- Provide `--help` examples in README for new functionality.
- Preserve backward compatibility with existing flags.
- Use JSON output only when explicitly requested (`--json`).

## Data & safety
- Be careful with any change that deletes or overwrites data in `~/.alwaysrunning/`.
- Prefer additive migration logic for new runtime formats.
- Avoid background processes spawning other background processes.

## Testing
- Keep tests local, fast, and deterministic.
- Prefer unit tests over integration tests.
- No network access or shelling out in tests.

## Docs
- Update README when behavior or CLI changes.
- Keep README concise and copy‑paste friendly.

## Release
- GitHub Actions builds on release creation.
- Do not change release workflow without strong reason.
