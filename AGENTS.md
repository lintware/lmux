# lmux – agent notes

## Deploying the download site (https://lmux.lintlabs.org)

- Site source: `deploy/site/` (index.html, vendor/, wrangler.jsonc). Built output: `target/site/public`.
- Full flow: `deploy/macos/bundle.sh`, then `deploy/site/publish.sh` (builds DMG + page, then runs `wrangler deploy --assets target/site/public`).
- Cloudflare CLI is `cf` (`cf --help`, `cf auth whoami`, `cf auth login`). Discover commands with `cf cli search "<task>"` rather than nested `--help`.
- The site lives in account `7efeb52ec5727a903edd8ef7ae8eb925` (the `account_id` in `deploy/site/wrangler.jsonc`).
- `cf deploy` has no `--assets` flag; it delegates to wrangler's experimental new-config flow (needs `package.json` with wrangler plus `cloudflare.config.ts`), which isn't set up here. The static-assets Worker is deployed with `wrangler deploy --assets`.
- If wrangler fails with "Failed to fetch auth token: 400", its own login expired. Skip `wrangler login`: the `cf` CLI stays logged in (`cf auth whoami`), and its OAuth token (`oauth_token` in `~/Library/Preferences/cloudflare/config/default.json`) works as `CLOUDFLARE_API_TOKEN`. `publish.sh` exports it automatically; by hand: `CLOUDFLARE_API_TOKEN=$(python3 -c "import json,os;print(json.load(open(os.path.expanduser('~/Library/Preferences/cloudflare/config/default.json')))['oauth_token'])") wrangler deploy --assets target/site/public` from `deploy/site`. If `cf` itself is logged out, run `cf auth login`.

## Updating remote hosts

- Remote daemons run the code from the last `deploy/install-remote.sh <ssh-target>` (builds on the host, restarts `lmuxd`; existing panes keep their environment, new panes pick up changes). Hosts are listed in `~/Library/Application Support/lmux/hosts.json`. Rerun it for each ssh host after changing `crates/` code.
- `publish.sh` takes `icon.png` from the built app bundle, so rebuild the bundle first to get the current icon.
- The DMG is not notarized unless a `lmux-notary` notarytool keychain profile exists.
