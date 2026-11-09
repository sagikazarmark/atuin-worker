# atuin-worker: deploy

Your own [Atuin](https://atuin.sh) sync server, for one person, on Cloudflare Workers.

[![Deploy to Cloudflare](https://deploy.workers.cloudflare.com/button)](https://deploy.workers.cloudflare.com/?url=https://github.com/sagikazarmark/atuin-worker/tree/main/deploy)

> [!WARNING]
> The default `download-worker.sh` installs the **latest GitHub release on every build**.
>
> A rebuild can therefore upgrade your Worker and database migrations.
>
> After the first deployment, [pin a version](#pin-a-version-and-upgrade-manually) by replacing the bootstrap with that release's
> `download-worker.sh` and committing it. Select and apply later upgrades manually.

The full documentation, including how to move from another sync server, what does not work and the
limits of the Cloudflare Free plan, is in the [main README](https://github.com/sagikazarmark/atuin-worker).

## Deploy with the button

**1. Click the button above.** Cloudflare asks you to log in, then shows a setup page:

- **Git account and repository name.** Cloudflare copies this folder into a new repository in your
  GitHub or GitLab account and deploys from it.
- **Worker name.** The default, `atuin`, gives you `https://atuin.<your-subdomain>.workers.dev`.
- **D1 database name.** The default, `atuin`, is fine. Cloudflare creates the database for you.
- **`ATUIN_USERNAME` and `ATUIN_PASSWORD`.** The username may contain letters, digits and hyphens.
  The password must be at least 16 characters, and should be generated rather than chosen, for
  example with `openssl rand -base64 24`: the server does not slow down password guesses, so a long
  random password is what keeps it safe. Do not use it anywhere else.
- **Build and deploy commands.** These are filled in from `package.json` (`npm run build` and
  `npm run deploy`). Keep them.

**2. Wait for the first build to finish.** The setup page links to its log. When it is done, your
Worker's address is shown in the Cloudflare dashboard under **Workers & Pages**.

**3. Check it.** Open `https://<worker-name>.<your-subdomain>.workers.dev/` in a browser. You should
see a short JSON message with a Terry Pratchett quote and the Atuin version.

**4. Pin the deployed version.** Find the `Downloaded atuin-worker v...` line in the build log and
follow [Pin a version and upgrade manually](#pin-a-version-and-upgrade-manually) using that tag.

## Deploy from the command line

You need [Node.js](https://nodejs.org) 22 or newer. Inside this folder:

```sh
npm install
npm run build
npx wrangler d1 create atuin --binding DB --update-config
npx wrangler secret put ATUIN_USERNAME
npx wrangler secret put ATUIN_PASSWORD
npm run deploy
```

The first time, wrangler asks you to log in to Cloudflare in your browser. It then deploys the
Worker, creates a D1 database called `atuin`, writes the database ID into `wrangler.toml`, and
applies the migrations. Keep the updated `wrangler.toml`: later upgrades use it to find the
database.

After the first deployment, [pin the downloaded version](#pin-a-version-and-upgrade-manually).

## Set up your machines

On each machine, set `sync_address` in `~/.config/atuin/config.toml` to your Worker's address and
log in:

```toml
sync_address = "https://<worker-name>.<your-subdomain>.workers.dev"
```

```sh
atuin login -u <username>
atuin sync
```

All your machines must use the same encryption key. On the first one, press Enter at the key
prompt; on the others, paste what `atuin key` prints on the first. See
[Set up your machines](https://github.com/sagikazarmark/atuin-worker#set-up-your-machines) for
details.

## Change the password

In the dashboard, open your Worker, go to **Settings > Variables and Secrets**, and edit
`ATUIN_PASSWORD`. Or, from this folder:

```sh
npx wrangler secret put ATUIN_PASSWORD
```

This logs out every machine: run `atuin logout` and `atuin login -u <username>` on each one again.
Your data and your encryption key are not affected.

## Pin a version and upgrade manually

Replace the bootstrap with the `download-worker.sh` attached to a specific GitHub release. To keep
the version from your first deployment, use the tag printed in its build log. Run these commands
inside this folder, replacing `vX.Y.Z` with the chosen tag:

```sh
version=vX.Y.Z
curl -fsSL --retry 3 -o download-worker.sh.new \
  "https://github.com/sagikazarmark/atuin-worker/releases/download/$version/download-worker.sh" &&
  sh download-worker.sh.new &&
  mv download-worker.sh.new download-worker.sh
```

This downloads and verifies the selected Worker's archive before replacing the script. Review the
change with `git diff -- download-worker.sh`, commit `download-worker.sh`, and push. Workers Builds
uses the deployment command from step 4. The release-specific script contains the version and
checksum, so subsequent builds keep using that release.

For later upgrades, repeat these steps with the new release tag, or run `npm run upgrade` to do the
same with the latest release. From the command line, run `npm run deploy` after the download
succeeds instead of pushing. It applies migrations before deploying the downloaded Worker.

## Settings

These are `[vars]` in `wrangler.toml`. Change them and push, or run `npx wrangler deploy`.

| Variable          | Default  | Meaning |
| ----------------- | -------- | ------- |
| `PAGE_SIZE`       | `100`    | Records per upload and download page, advertised to clients. |
| `MAX_RECORD_SIZE` | `262144` | Largest accepted record payload in bytes (256 KiB). Real records are a few KB. |

`PAGE_SIZE × MAX_RECORD_SIZE` must not exceed 40 MiB, because D1 refuses query results over 50 MB.
