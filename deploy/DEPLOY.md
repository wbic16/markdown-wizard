# Deploying md.phext.io (awsbox)

One run, as a sudo-capable login on the box:

```
git clone https://github.com/wbic16/markdown-wizard.git /source/markdown-wizard
/source/markdown-wizard/deploy/install.sh
```

DNS for `md.phext.io` already points at the box (Will, 2026-11-15).
Everything else is in the script. Sudo on awsbox is a human's
keystroke by house rule; the script uses `sudo` for system steps and
never handles a password itself.

## What install.sh does, in order

1. **Source** — clones this repo to `/source/markdown-wizard` if it
   is not there (a public repo; plain HTTPS clone). An existing
   checkout is used as it stands.
2. **Build** — `cargo build --release` on the box, the same way
   phext-edit is built there. The binary lands at
   `/source/markdown-wizard/target/release/markdown-wizard`.
3. **Data** — creates `/source/markdown-wizard-data`. One directory
   per published slug lives here (`bundle.zip`, `meta.txt`, `prev/`,
   and `site/` — the derived outputs: pre-rendered pages, PDFs, and
   the static-site download, rebuilt at every publish); nothing else
   writes to it.
4. **Service** — installs `deploy/markdown-wizard.service`, enables
   and restarts it, and waits for `GET /api/status` on
   `127.0.0.1:8090` (phext-edit holds 8080, the preview 8081; 8090 is
   this service's).
5. **Site** — if no Let's Encrypt certificate exists yet, installs
   the HTTP bootstrap site (`nginx-md.phext.io.bootstrap.conf`),
   reloads nginx, and runs certbot against the live block; then
   installs the TLS site (`nginx-md.phext.io.conf`) either way,
   tests the config, and reloads.
6. **Smoke** — `https://md.phext.io/api/status` must answer and the
   editor must load at `https://md.phext.io/`.

## PDF output needs a Chromium

Each published file's PDF is printed at publish time by a headless
Chromium — the one external tool the service shells out to. The
server looks for `chromium`, `chromium-browser`, `google-chrome`,
`google-chrome-stable`, or `chrome-headless-shell` on the PATH
(the `MARKDOWN_WIZARD_CHROMIUM` environment variable names a
specific binary, and the Playwright cache under the service user's
home is checked last). `install.sh` checks after the build and
warns when none is found; it does not fail the install. Without a
Chromium the service runs whole — bundles publish, pages render,
the static site downloads — but there are no PDFs, and each `.pdf`
address answers with a page saying so. Installing one later (e.g.
`apt install chromium`) is enough: the next publish prints, and
bundles already published gain their PDFs at the next server
start, when missing outputs are rebuilt.

## Verify afterwards

- `systemctl status markdown-wizard` — active, journaling.
- `https://md.phext.io/` — the editor, a file already open.
- Publish a throwaway bundle from the editor and read it back at its
  `/b/<slug>/` address; delete its directory under
  `/source/markdown-wizard-data` afterwards if it was only a test.

## Federation (sign-in with edit.phext.io)

Federation is on by default and needs no deploy step:

- **Callback URL** — `https://md.phext.io/auth/callback`. The
  server derives it per request from the `Host` and
  `X-Forwarded-Proto` headers, which the shipped nginx site
  already passes; `/auth/*` and the vault endpoints ride the
  existing `location /` proxy, so no new nginx location exists.
- **Registration** — none by hand. At the first sign-in the
  server registers itself with edit.phext.io's dynamic client
  registration as a public client (no secret; PKCE S256 is the
  credential) and caches the client id in its state. If the
  provider ever answers `invalid_client`, the cached
  registration is dropped and the next sign-in re-registers.
- **State** — `<data>/fed/state.json` (mode 0600) holds the
  cached registration and every live session's access/refresh
  token pair. **Treat it like a secret**: it is the one file
  that can act as a signed-in user, and it belongs in the data
  directory's backups. Losing it signs everyone out and nothing
  more; the vaults live on edit.phext.io, untouched.
- **Env** — `MDWIZ_FEDERATION=off` disables the door;
  `MDWIZ_PHEXT_ISSUER` overrides the provider (default
  `https://edit.phext.io`).
- **One provider-side condition** — edit.phext.io's MCP gate
  requires the signed-in account to be on its editor list; a
  user outside it can complete sign-in but vault calls answer
  403, and the editor surfaces that answer rather than hiding
  it.

## Roll back

```
sudo systemctl disable --now markdown-wizard
sudo rm /etc/nginx/sites-enabled/md.phext.io /etc/nginx/sites-available/md.phext.io
sudo nginx -t && sudo systemctl reload nginx
```

The data directory is left in place on purpose: unpublishing the
site must not destroy anyone's bundles. Remove
`/source/markdown-wizard-data` by hand only when that is truly meant.
