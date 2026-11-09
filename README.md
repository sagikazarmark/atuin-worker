# atuin-worker

[![GitHub Workflow Status](https://img.shields.io/github/actions/workflow/status/sagikazarmark/atuin-worker/ci.yaml?style=flat-square)](https://github.com/sagikazarmark/atuin-worker/actions/workflows/ci.yaml)
[![OpenSSF Scorecard](https://api.securityscorecards.dev/projects/github.com/sagikazarmark/atuin-worker/badge?style=flat-square)](https://securityscorecards.dev/viewer/?uri=github.com/sagikazarmark/atuin-worker)

[![Deploy to Cloudflare](https://deploy.workers.cloudflare.com/button)](https://deploy.workers.cloudflare.com/?url=https://github.com/sagikazarmark/atuin-worker/tree/main/deploy)

Your own [Atuin](https://atuin.sh) sync server, for one person, on Cloudflare Workers and D1.

It speaks the same sync protocol as the official server, so the stock `atuin` client (18.24.4)
works unchanged: you point it at your Worker and run `atuin login`. There are no accounts to manage.
The Worker has exactly one user, defined by two secrets, and stores only the encrypted records your
machines upload. The server never sees your encryption key.

- [What you need](#what-you-need)
- [Deploy](#deploy)
- [Set up your machines](#set-up-your-machines)
- [Move from another sync server](#move-from-another-sync-server)
- [Change the password](#change-the-password)
- [Upgrade](#upgrade)
- [What does not work](#what-does-not-work)
- [The Cloudflare Free plan](#the-cloudflare-free-plan)
- [Settings](#settings)
- [Logs and backups](#logs-and-backups)
- [Development](#development)

## What you need

- A Cloudflare account. The Free plan is enough for normal use (see
  [The Cloudflare Free plan](#the-cloudflare-free-plan)).
- [Node.js](https://nodejs.org) 22 or newer, which provides `npm` and `npx`.
- To deploy from the command line, this repository's [`deploy/`](deploy) folder. It holds the
  configuration (`wrangler.toml`, `package.json`); `npm run build` inside it downloads the Worker
  from the latest release. You do not need Rust.

Each release's `worker.tar.gz` holds the Worker (`build/index.js` and `build/index_bg.wasm`, with
its license in `build/LICENSE`) and the database migrations (`migrations/`), with no folder around
them: unpacked inside `deploy/`, it replaces those. Its `download-worker.sh` downloads that
release's `worker.tar.gz`, checks it and unpacks it. To check a download yourself, compare
`sha256sum worker.tar.gz` with the `.sha256` file next to it. Releases are immutable, and each
`worker.tar.gz` has a build provenance attestation from the release workflow, also attached as
`worker.tar.gz.sigstore.json`. To check them, with the [GitHub CLI](https://cli.github.com):

```sh
# worker.tar.gz was built by this repository's release workflow
gh attestation verify worker.tar.gz --repo sagikazarmark/atuin-worker \
  --signer-workflow sagikazarmark/atuin-worker/.github/workflows/release.yaml

# the same, with the attached attestation instead of the one stored on GitHub
gh attestation verify worker.tar.gz --bundle worker.tar.gz.sigstore.json \
  --repo sagikazarmark/atuin-worker \
  --signer-workflow sagikazarmark/atuin-worker/.github/workflows/release.yaml

# worker.tar.gz is an asset of release vX.Y.Z, as GitHub recorded the release when it was published
gh release verify-asset vX.Y.Z worker.tar.gz --repo sagikazarmark/atuin-worker
```

## Deploy

[![Deploy to Cloudflare](https://deploy.workers.cloudflare.com/button)](https://deploy.workers.cloudflare.com/?url=https://github.com/sagikazarmark/atuin-worker/tree/main/deploy)

The button downloads the latest prebuilt release during the build; [deploy/README.md](deploy/README.md)
walks through deployment and pinning a version afterward.
To deploy from the command line instead, inside the `deploy/` folder:

**1. Download the Worker, create the database and deploy.**

```sh
npm install
npm run build
npx wrangler d1 create atuin --binding DB --update-config
npm run deploy
```

`npm run build` downloads the latest release's Worker and migrations. The first time, wrangler asks
you to log in to Cloudflare in your browser. `d1 create` creates a D1 database called `atuin` and
writes its ID into `wrangler.toml`; `npm run deploy` applies the database migrations, then deploys
the Worker. When it finishes, it prints your Worker's address, something like
`https://atuin.<your-subdomain>.workers.dev`. Keep the updated `wrangler.toml`: later upgrades
use it to find the database.

If syncing fails with a server error such as "failed to calculate record index", the migrations did
not run: apply them with `npx wrangler d1 migrations apply DB --remote`.

D1 keeps the database in one region, chosen near you when it is created. If your machines are
somewhere else, add a [location hint](https://developers.cloudflare.com/d1/configuration/data-location/)
such as `weur` or `enam` to the create command: `--location weur`.

**2. Set your username and password.**

```sh
npx wrangler secret put ATUIN_USERNAME
npx wrangler secret put ATUIN_PASSWORD
```

Each command prompts for the value. The username may contain letters, digits and hyphens. The
password must be at least 16 characters, and should be generated rather than chosen, for example
with `openssl rand -base64 24`: the server does not slow down password guesses with a password
hash, so a long random password is what keeps it safe. Do not use it anywhere else.

Until both secrets are set, every request except the health check answers with an error that says
which one is missing.

**3. Check it.** Open `https://atuin.<your-subdomain>.workers.dev/` in a browser. You should see a
short JSON message with a Terry Pratchett quote and the version, `18.24.4`.

## Set up your machines

On each machine, edit `~/.config/atuin/config.toml` and set:

```toml
sync_address = "https://atuin.<your-subdomain>.workers.dev"
```

Then log in:

```sh
atuin login -u <username>
```

It asks for your password, then for your encryption key. All your machines must use the same key:

- On your **first** machine, press Enter at the key prompt to keep the key Atuin already uses
  there. (Atuin creates it the first time it records a command; if the prompt says no key file was
  found, open a new shell, run any command, and try again.)
- On every **other** machine, run `atuin key` on the first one and paste what it prints. A machine
  that has synced before with the same key, with any server, can just press Enter.

Then run `atuin sync`. From now on Atuin syncs in the background as usual.

`atuin login` also reads the password from the `ATUIN_PASSWORD` environment variable if it is
set, which is handy for scripts.

## Move from another sync server

There is no import tool, and you do not need one: every machine that has been syncing holds a
complete copy of everything in your account, including the history of your other machines.

1. Pick the machine that has synced most recently. Run `atuin sync` against the old server one last
   time.
2. Change its `sync_address` to your Worker, then `atuin logout` and `atuin login -u <username>`,
   pressing Enter at the key prompt to keep your key.
3. Run `atuin sync`. It uploads everything. If your history is large, read
   [The Cloudflare Free plan](#the-cloudflare-free-plan) first: on Free, a big upload takes more
   than one day.
4. Move your other machines the same way.

Keep the old server until a second machine has pulled your full history from the Worker
(`atuin history list` shows it).

## Change the password

```sh
npx wrangler secret put ATUIN_PASSWORD
```

This takes effect within seconds and logs out **every** machine: each one then fails to sync with
"session not found" until you run `atuin logout` and `atuin login -u <username>` on it again. Your
data and your encryption key are not affected.

Do this if a machine is lost or stolen. There is no way to log out a single machine: they all share
one session, which is derived from the password. That session, stored in Atuin's data directory
(`~/.local/share/atuin`) on each machine, is as good as the password for syncing.

You can change `ATUIN_USERNAME` the same way, with the same effect.

## Upgrade

With the button, follow [Pin a version and upgrade manually](deploy/README.md#pin-a-version-and-upgrade-manually).

From the command line, inside your folder, run the new release's `download-worker.sh` (or unpack its
`worker.tar.gz` there), so the new `build/` and `migrations/` replace the old ones. If the new
release's `deploy/wrangler.toml` differs from yours, merge the changes into yours by hand and keep
your `database_id` line. Then:

```sh
npm run deploy
```

This applies any new database migrations first, then deploys the new Worker. Migrations only ever
add to the schema, so the old Worker keeps working while they run.

When a new **major** version of Atuin comes out (19.0.0, say), clients on that version refuse to
sync with a server that advertises an older major version. Each release of atuin-worker advertises
the Atuin version it was tested against, so you then need to install a release that tracks the new
one.

## What does not work

This server is for one person and does only what syncing needs:

- `atuin register`: there is nothing to register; set the secrets instead.
- `atuin account change-password`: change the `ATUIN_PASSWORD` secret instead.
- `atuin account delete`: to replace everything on the server with one machine's copy, run
  `atuin store push --force` on that machine. To remove everything, delete the Worker and the
  database in the Cloudflare dashboard.
- More than one user.
- Atuin Hub features, and packfile sync. The client falls back to normal record sync on its own.

## The Cloudflare Free plan

For everyday use the Free plan is plenty. A machine syncing every few minutes uses a small fraction
of the daily allowances.

The limit that matters is D1's **100,000 rows written per day**. Each record uploaded costs about two
rows, so on Free you can upload roughly **50,000 records per day**. The counter resets at
**00:00 UTC**. When it is used up, uploads fail until the reset; the client just reports a sync
error, and the next sync after the reset continues where it stopped. Nothing is lost or duplicated.

That only matters in three situations:

1. **The first upload of a large history.** Moving 200,000 records takes about four days on Free.
   Leave the machine syncing as usual and it finishes on its own.
2. **`atuin store push --force` of a large store.** It first deletes everything on the server, and
   deleting counts as writing: deleting 200,000 records uses most of a day's allowance before the
   re-upload starts. If it fails partway, run it again after 00:00 UTC.
3. **Hitting the reset.** Sync errors that start mid-upload and stop after 00:00 UTC are the
   limit, not a bug.

To see how big your history is before moving it, run `atuin store status`, which lists the records
per host and type.

If you do not want to wait, switch to the Workers Paid plan for a day: the limit there is far
higher, and a large upload finishes in minutes. You can switch back afterwards.

## Settings

These are `[vars]` in `wrangler.toml`. After changing one, run `npx wrangler deploy`.

| Variable          | Default  | Meaning |
| ----------------- | -------- | ------- |
| `PAGE_SIZE`       | `100`    | Records per upload and download page, advertised to clients. |
| `MAX_RECORD_SIZE` | `262144` | Largest accepted record payload in bytes (256 KiB). Real records are a few KB. |

`PAGE_SIZE × MAX_RECORD_SIZE` must not exceed 40 MiB, because D1 refuses query results over
50 MB. If a setting is invalid, every request answers with an error naming it.

The login rate limit is the `[[ratelimits]]` binding: five login attempts per minute. Syncing is not
limited. The limit is shared by everyone, so while someone is guessing passwords, your own
`atuin login` may be refused with "Rate limited"; already logged-in machines keep syncing.

## Logs and backups

The Worker writes one line per request to
[Workers Logs](https://developers.cloudflare.com/workers/observability/logs/workers-logs/) (in the
Cloudflare dashboard, under your Worker): the route, the status, the number of records and how long
it took. It never logs your password, session or data.

Every machine you sync holds a complete copy of your records, so if the database is lost, logging
in from any machine and running `atuin sync` restores it. On top of that, D1's
[Time Travel](https://developers.cloudflare.com/d1/reference/time-travel/) can restore the
database to any point in the last 7 days on Free (30 on Paid), and you can save a copy whenever you
like:

```sh
npx wrangler d1 export DB --remote --output atuin.sql
```

## Development

The Worker is Rust compiled to WebAssembly with
[workers-rs](https://github.com/cloudflare/workers-rs). Everything except `src/worker.rs` is plain
Rust over the `http` crate's types and is tested natively; `src/worker.rs` is the glue to the
Workers runtime, D1 and the rate limiter.

| Path | Contents |
| ---- | -------- |
| `src/wire.rs` | The protocol's JSON types, vendored from upstream `atuin-domain` |
| `src/caps.rs` | The capability document and its token |
| `src/auth.rs` | The config check, the session token, `Authorization` and login |
| `src/proto.rs` | Routing, settings, capability negotiation, fixed answers, response headers |
| `src/sql.rs` | The SQL, and the helpers around it |
| `src/worker.rs` | The fetch handler, D1 and the rate limiter (wasm only) |
| `migrations/` | The D1 schema |
| `tests/drift.rs` | The vendored types and capability token against upstream |
| `tests/e2e.rs` | Direct HTTP and the real `atuin-client` against `wrangler dev --local` |
| `scripts/` | Release assembly, the end-to-end runner and the CLI smoke test |

Builds and checks run through [just](https://just.systems). With [devenv](https://devenv.sh),
`devenv shell` provides everything: just, the Rust toolchain from `rust-toolchain.toml` (with the
`wasm32-unknown-unknown` target), `worker-build` at the version in `Cargo.lock`, Node.js 22 with
wrangler installed into `deploy/node_modules`, cargo-audit, cargo-deny, OpenSSL headers for the
native tests, and the atuin 18.24.4 client for the smoke test (from
[nixpkgs-multiverse](https://github.com/fzakaria/nixpkgs-multiverse)). CI runs the same recipes in
the same shell.

```sh
just                     # list the recipes
just ci                  # format, clippy (native and wasm32), unit and drift tests, docs, audit, deny
just rust::test          # a single native Rust check (rust::fmt, clippy, test, doc, audit, deny)
just rust::doc --open    # build and open the API documentation
just release             # build with worker-build and package dist/worker.tar.gz
just e2e dist/worker.tar.gz     # start three local servers, run tests/e2e.rs
just smoke dist/worker.tar.gz   # the real CLI, two machines
```

The root `wrangler.toml` is for development. To run the Worker locally, copy `.dev.vars.example` to
`.dev.vars`, fill it in, then, from the repository root:

```sh
npx --prefix deploy wrangler d1 migrations apply DB --local
npx --prefix deploy wrangler dev
```

Its `[build]` stanza compiles the Worker with `worker-build` first, so
`npx --prefix deploy wrangler deploy` from the root also deploys straight from a checkout.

`deploy/` is the source for the Deploy to Cloudflare button. The button copies only that folder
into the deployer's repository, so it has its own `wrangler.toml` (without `[build]`, because
Workers Builds has no Rust toolchain), `package.json`, `.dev.vars.example` and
[README](deploy/README.md). Keep its `wrangler.toml` in step with the root one. The end-to-end and
smoke tests run the Worker under `deploy/wrangler.toml`, the way a deployer has it.

To release, run the Release workflow (`.github/workflows/release.yaml`) from `main`, in the Actions
tab or with `gh workflow run release.yaml -f bump=minor` (`patch`, `minor` or `major` from the
latest `vX.Y.Z` tag; the first release counts from `v0.0.0`), or `-f version=vX.Y.Z` for an exact
version, which must be newer than the latest. `bump` defaults to `none`, so a run with neither
fails instead of releasing. The workflow builds with the committed `Cargo.lock`
and packages `worker.tar.gz` (`just release <version>`: `build/index.js`, `build/index_bg.wasm`,
`build/LICENSE` and `migrations/`, nothing else of worker-build's output), runs the end-to-end and
smoke tests on that archive and attests its build provenance, then publishes the GitHub Release
with the archive, its checksum, its attestation and `download-worker.sh`; publishing creates the
version tag on the tested commit. Nothing is tagged or published if a build, test or attestation
step fails. As an escape hatch, `-f skip-tests=true` releases without the end-to-end and
smoke tests.
Publishing is a job of its own: if it fails, re-run only that job, without attesting again. gh
deletes its draft when an upload fails; only if that cleanup fails too, or the job is cancelled
mid-upload, is a draft left behind to delete by hand.

The repository needs two settings for this: immutable releases, so that a published release's
assets and tag cannot change (GitHub then also attests the release itself, which
`gh release verify` checks), and a tag ruleset that lets only the release workflow create `v*`
tags and nobody update or delete them.

## License

Licensed under the GNU Affero General Public License, Version 3 only
([LICENSE](LICENSE) or <https://www.gnu.org/licenses/agpl-3.0.html>).

The deployment template, the [`deploy/`](deploy) folder, is licensed under the BSD Zero Clause
License ([deploy/LICENSE](deploy/LICENSE) or <https://opensource.org/license/0bsd>), and so is
[`scripts/download-worker.sh.tmpl`](scripts/download-worker.sh.tmpl), which each release's
`download-worker.sh` is made from. Your copy of the template is yours: change it and share it as you
like. The Worker it downloads stays under the AGPL; each release's `worker.tar.gz` carries the
license as `build/LICENSE`.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you shall be licensed as above, without any
additional terms or conditions: contributions to `deploy/` and
`scripts/download-worker.sh.tmpl` under the BSD Zero Clause License, everything else under the
AGPL.
