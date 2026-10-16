# lmux – agent notes

## Deploying the download site (https://lmux.lintlabs.org)

- Site source: `deploy/site/` (index.html, vendor/, wrangler.jsonc). Built output: `target/site/public`.
- Full flow: `deploy/macos/bundle.sh`, then `deploy/site/publish.sh` (builds DMG + page, then runs `wrangler deploy --assets target/site/public`).
- Cloudflare CLI is `cf` (`cf --help`, `cf auth whoami`, `cf auth login`). Discover commands with `cf cli search "<task>"` rather than nested `--help`.
- The site lives in account `7efeb52ec5727a903edd8ef7ae8eb925` (the `account_id` in `deploy/site/wrangler.jsonc`).
- `cf deploy` has no `--assets` flag; it delegates to wrangler's experimental new-config flow (needs `package.json` with wrangler plus `cloudflare.config.ts`), which isn't set up here. The static-assets Worker is deployed with `wrangler deploy --assets`.
- If wrangler fails with "Failed to fetch auth token: 400", its own login expired. Skip `wrangler login`: the `cf` CLI stays logged in (`cf auth whoami`), and its OAuth token (`oauth_token` in `~/Library/Preferences/cloudflare/config/default.json`) works as `CLOUDFLARE_API_TOKEN`. `publish.sh` exports it automatically; by hand: `CLOUDFLARE_API_TOKEN=$(python3 -c "import json,os;print(json.load(open(os.path.expanduser('~/Library/Preferences/cloudflare/config/default.json')))['oauth_token'])") wrangler deploy --assets target/site/public` from `deploy/site`. If `cf` itself is logged out, run `cf auth login`.

## Updating remote hosts

- GitHub is the single source. Every host keeps its own clone in `~/.local/share/lmux/src` and updates itself with `deploy/update.sh`: fetch the ref it follows, build (on Macs with lmux.app installed, the app too), install, restart the daemon, then check that the daemon answers with the new build and that every tmux pane and session is still there; otherwise it rolls back and holds automatic updates (`~/.local/state/lmux/update.held`). An hourly systemd timer / launchd agent (`--install-timer`) runs it with `--auto`. Nothing is copied from one server to another; to ship a change, push it to the ref the hosts follow (`LMUX_UPDATE_REF` in `~/.config/lmux/update.env`).
- Per host: `~/.local/share/lmux/src/deploy/update.sh --status` (what it runs and follows), `--check`, or a plain run to update now. Never point the updater at `~/lmux`.
- New host: `deploy/install-remote.sh <ssh-target> [ref]` (the host clones from GitHub, builds, sets up `lmuxd`, hooks and automatic updates). Hosts are listed in `~/Library/Application Support/lmux/hosts.json` and `~/.ssh/config`; prefer tailnet addresses there (`lmux list-hosts` shows how each route travels).
- `lmux --version` and `lmux rpc system.ping` report the commit a binary and a running daemon were built from.
- `publish.sh` takes `icon.png` from the built app bundle, so rebuild the bundle first to get the current icon.
- The DMG is not notarized unless a `lmux-notary` notarytool keychain profile exists.
