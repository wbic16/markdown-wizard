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
   per published slug lives here (`bundle.zip`, `meta.txt`, `prev/`);
   nothing else writes to it.
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

## Verify afterwards

- `systemctl status markdown-wizard` — active, journaling.
- `https://md.phext.io/` — the editor, a file already open.
- Publish a throwaway bundle from the editor and read it back at its
  `/b/<slug>/` address; delete its directory under
  `/source/markdown-wizard-data` afterwards if it was only a test.

## Roll back

```
sudo systemctl disable --now markdown-wizard
sudo rm /etc/nginx/sites-enabled/md.phext.io /etc/nginx/sites-available/md.phext.io
sudo nginx -t && sudo systemctl reload nginx
```

The data directory is left in place on purpose: unpublishing the
site must not destroy anyone's bundles. Remove
`/source/markdown-wizard-data` by hand only when that is truly meant.
