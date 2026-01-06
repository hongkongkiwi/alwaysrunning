# alwaysrunning

Run a binary continuously on a VM. No YAML. No unit files. No Kubernetes.

`runner` is a tiny supervisor that keeps your binary alive, restarts it if it dies,
and writes logs to `~/.alwaysrunning`. It’s meant to be the simplest possible
“run this and keep it alive” tool.

## Why

You want a long‑running binary with:
- Keep‑alive + restart on crash
- Start on boot
- Logs you can tail
- Minimal surface area

That’s it.

## Install

### From release

Download a prebuilt `runner` binary from the GitHub Releases page and put it in your PATH.

Supported OS: macOS, Linux, Windows. Autostart setup is currently macOS/Linux only.

### From source

```bash
cargo build --release
```

Binary path: `target/release/runner`

## Quickstart

```bash
./target/release/runner run myapp ./my-binary --instances 3
./target/release/runner status
./target/release/runner status --watch
./target/release/runner status --json
./target/release/runner logs myapp --follow
./target/release/runner logs myapp --lines 500
./target/release/runner logs myapp --since 10m
./target/release/runner logs myapp --json
./target/release/runner stop myapp
./target/release/runner start myapp
./target/release/runner restart myapp
./target/release/runner delete myapp
```

Notes:
- `stop` pauses processes but keeps the app registered (use `start` to resume).
- `delete` removes the app configuration entirely.

## Environment variables

Load environment variables from a file:

```bash
./target/release/runner run myapp ./my-binary --env-file .env
./target/release/runner run myapp ./my-binary --env-file .env --clean-env
```

Env file format: simple `KEY=VALUE` lines, optional `export` prefix, `#` comments, and optional quotes.

Note: runner flags must come before the binary args. Use `--` if your binary has flags that
would conflict with runner or to force everything after it to be passed through:

```bash
./target/release/runner run myapp ./my-binary --env-file .env -- --binary-flag foo
```

## Foreground / attach

Foreground mode (live screen in current terminal):

```bash
./target/release/runner run myapp ./my-binary --instances 3 --foreground
```

Attach to a running instance from another terminal:

```bash
./target/release/runner attach
```

Watch screen controls: `q`/Ctrl+C to quit, `r` to restart, `s` to stop,
`j`/`k` to select app, `[`/`]` to select instance, `l` to toggle log tail.

## Export / import config

Export/import config:

```bash
./target/release/runner export runner.json
./target/release/runner import runner.json --start
```

## Autostart (boot)

`runner` can install autostart on macOS (launchd) and Linux (systemd user).

```bash
./target/release/runner install
```

This writes a minimal, hidden OS-native config and enables it for you.
You don't have to hand-edit anything. Remove it with:

```bash
./target/release/runner uninstall
```

If you'd prefer to skip autostart:

```bash
./target/release/runner run myapp ./my-binary --instances 3 --no-autostart
```

## Logs

Each instance writes to:

```
~/.alwaysrunning/apps/<app>/logs/instance-<n>.log
```

Filters:
- `--since` accepts Unix seconds or a duration like `10m`, `2h`, `1d` (coarse; uses file mtime).
- `--json` prints JSON lines: `{"line":"..."}`.

## Status

Human‑readable:

```bash
runner status
runner status --watch
```

JSON (useful for scripts):

```bash
runner status --json
```

## Signals

Send a different signal when stopping/restarting:

```bash
runner stop myapp --signal KILL
runner restart myapp --signal HUP
```

On Windows, signals are accepted but behave as a forceful terminate.

## Windows notes

- Autostart (`runner install`) is not supported on Windows yet.
- `--signal` is treated as a forceful terminate.
- Process liveness and exit codes are derived from Windows process APIs.

## Files & data layout

Runner stores state in your home directory:

```
~/.alwaysrunning/
  state.json        # app definitions
  runtime.json      # process metadata
  run/daemon.pid    # daemon PID
  apps/<name>/logs/ # per‑instance logs
```

## Security notes

- `--clean-env` clears inherited environment variables before applying `--env-file`.
- Runner does not sandbox your process. It executes your binary directly.

## What this is (and isn't)

- It is a single-VM supervisor with a dead-simple CLI.
- It is not a multi-VM orchestrator. To scale horizontally, bake this into your image or
  cloud-init and use your VM autoscaler.

## Contributing

PRs welcome. Keep it simple and low‑dependency. Run tests with:

```bash
cargo test
```

## Releasing

Create and push a semantic tag. The GitHub Actions workflow will build and attach
binaries automatically.

```bash
git tag v0.1.2
git push origin v0.1.2
```

Manual release: trigger the `manual-release` workflow in GitHub Actions and
enter a version like `0.1.2`. It will create the tag and kick off the build.

## License

MIT
