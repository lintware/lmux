# Contributing to lmux

Thanks for wanting to help. lmux is small on purpose, so most contributions are a bug fix, a missing cmux behaviour, or a feature that costs nothing when it isn't used. This page is how to get a change from your machine into `dev` and into a release.

## Ways to help

- **Report a bug.** Open an [issue](https://github.com/lintware/lmux/issues/new/choose) with the version (`lmux --version`), the OS of the laptop and the host, and the shortest steps that show it. A pane that came back wrong after a restart is the most useful kind of report: say what was running in it.
- **Pick up a small one.** Look through the open issues for one that's well scoped, or take an item from the roadmap in the [README](README.md#status-and-roadmap). Say so on the issue first so two people don't build the same thing.
- **Try it on a host we haven't.** The daemon runs on Linux and macOS; each distro and tmux version finds something new. A note in an issue about what worked is a contribution.
- **Improve the docs.** If something here or in the README sent you the wrong way, fix it in the same pull request as whatever you were doing.

For anything bigger than a bug fix, open an issue first and say what you want to change and why. A short thread up front saves a rewrite later.

## What a change has to respect

lmux's reason to exist is that it's lean and that terminals outlive the window. Changes are reviewed against these four things:

1. **The laptop pays only for what's on screen.** A hidden pane streams nothing and holds no big state. A feature should cost the app nothing, in CPU or memory, when it isn't visible. If it adds a timer, a thread, or a per-surface cache, say what bounds it.
2. **The host owns the state.** Terminals live in tmux on their host and the daemon records the tree. The app is a viewer that can quit at any point. Anything the app alone knows is lost on quit, so keep it out of the path that restores panes.
3. **cmux command names stay.** The CLI is a drop-in for cmux's agent-control commands. Add commands freely; don't rename or change the meaning of existing ones.
4. **The daemon builds with Rust 1.88** (`rust-version` in `Cargo.toml`) because the servers it runs on are not all current. Don't use newer language or std features in `crates/`.

The numbers in the README ([why it's lean](README.md#why-its-lean)) are measured with `deploy/bench/stress.py`. If your change touches the app's draw loop, the terminal view or the wire protocol, run it before and after and put both results in the pull request.

## Set up

You need:

| For | You need |
|---|---|
| Everything | Rust 1.88 or newer (`rustup`), and `tmux` on your `PATH` (the servers run 3.3; the app ships 3.8) |
| The macOS app | macOS 12+, Xcode command line tools (`xcode-select --install`) |
| Browser panes on Linux | `python3-gi`, `gir1.2-webkit2-4.1`, `xvfb` |

```sh
git clone https://github.com/lintware/lmux
cd lmux
git switch dev

cargo build                    # the daemon and CLI (crates/)
(cd app && cargo build)        # the macOS app, a separate crate
```

The app is its own crate under `app/`, not a member of the root workspace, so `cargo` at the root never builds it. The two share only `crates/lmux-proto`.

### Run your build without touching your real terminals

A second daemon with its own socket, tmux server and state directory won't see or disturb the lmux you use every day:

```sh
export LMUX_SOCKET=/tmp/lmux-dev.sock
target/debug/lmux server \
  --socket "$LMUX_SOCKET" --tmux-socket lmux-dev \
  --state-dir /tmp/lmux-dev-state --ws-addr 127.0.0.1:7899 --no-browser &

target/debug/lmux new-workspace --name scratch --cwd /tmp   # talks to the dev daemon
```

Run `lmux --help` for the commands, and see [PLAN.md](PLAN.md) for how the daemon, the app and tmux fit together before you change the session tree or the protocol.

To try the app against a build, run `LMUX_SKIP_INSTALL=1 deploy/macos/bundle.sh`. It assembles and signs `target/bundle/lmux.app` without replacing the lmux in `/Applications` or `~/.local/bin`; open it with `open target/bundle/lmux.app`. Without the variable it installs both.

## Tests

Run both before you push; they are what a reviewer will run.

```sh
cargo test --workspace          # daemon, CLI and protocol (needs tmux)
(cd app && cargo test)          # the app, including UI snapshot tests
```

- The daemon tests start real tmux servers on scratch sockets and remove them afterwards. If one hangs and leaves a server behind, `tmux -L <socket> kill-server` cleans it up.
- UI changes are checked against images in `app/tests/snapshots/`. When a change to the look is intended, regenerate them with `UPDATE_SNAPSHOTS=1 cargo test --test snapshots` (from `app/`), look at the images that changed, and commit them with the change.
- A bug fix comes with a test that fails without it. If the bug only shows under a real tmux restart or a slow link, say how you reproduced it in the pull request.
- Match the style of the file you're in. There is no formatter gate; don't reformat code you aren't changing.

## Pull requests

1. **Branch from `dev`** and open the pull request against `dev`. `main` is what releases are cut from and only moves when `dev` is stable.
2. **One change per pull request.** A fix and a refactor are two pull requests.
3. **Write the commit message for the next person to debug it.** The subject says what's different for a user, in a sentence ("A closed workspace stays closed under load"). The body says what was wrong and why this is the fix, and what you measured. Look at `git log` for the tone.
4. **Keep the history readable.** Squash fixups before asking for review; separate commits are fine when each one stands on its own.
5. **Fill in the pull request template**: what changed, how you tested it, and, for anything the user sees, a screenshot or a short recording.
6. **Expect a request to cut it down.** A maintainer may ask you to drop part of a change to keep lmux lean. That's about the budget, not about your work.

By contributing you agree that your work is licensed under the [MIT License](LICENSE), the same as the rest of the project.

## Releases

Releases are cut from `main`, are numbered `vMAJOR.MINOR.PATCH`, and are listed on the [releases page](https://github.com/lintware/lmux/releases) with a `lmux-<version>-macos-arm64.dmg` for the app, a tarball with the `lmux` CLI and daemon, and checksums.

Every user-visible change goes in [CHANGELOG.md](CHANGELOG.md) under **Unreleased**, in the pull request that makes it.

Maintainers cut a release with:

```sh
# 1. set the version in Cargo.toml (workspace) and app/Cargo.toml, move
#    CHANGELOG.md's Unreleased notes under the new version, commit on dev,
#    and fast-forward main to it
# 2. tag and publish
deploy/release.sh v0.2.0
```

`deploy/release.sh` checks that the tree is clean and the version matches the tag, builds and signs the app, packages it, and creates the GitHub release with the checksums and the changelog notes.
