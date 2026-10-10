# lmux – agent notes

## Deploying the download site (https://lmux.lintlabs.org)

- Site source: `deploy/site/` (index.html, vendor/, wrangler.jsonc). Built output: `target/site/public`.
- Full flow: `deploy/macos/bundle.sh`, then `deploy/site/publish.sh` (builds DMG + page, then runs `wrangler deploy --assets target/site/public`).
- Cloudflare CLI is `cf` (`cf --help`, `cf auth whoami`, `cf auth login`). Discover commands with `cf cli search "<task>"` rather than nested `--help`.
- The site lives in account `7efeb52ec5727a903edd8ef7ae8eb925` (the `account_id` in `deploy/site/wrangler.jsonc`).
- `cf deploy` has no `--assets` flag; it delegates to wrangler's experimental new-config flow (needs `package.json` with wrangler plus `cloudflare.config.ts`), which isn't set up here. The static-assets Worker is deployed with `wrangler deploy --assets`.
- If wrangler fails with "Failed to fetch auth token: 400", its login expired: re-authenticate with `wrangler login`.
- `publish.sh` takes `icon.png` from the built app bundle, so rebuild the bundle first to get the current icon.
- The DMG is not notarized unless a `lmux-notary` notarytool keychain profile exists.
