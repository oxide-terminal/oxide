# Releasing Oxide

The loop: **note changes in `CHANGELOG.md` → `bump.sh` → commit → `release.sh`**.

## Steps

### 1. Land the feature work

Commit and push your changes as usual, with whatever descriptive messages you
like. Add a line for anything user-visible under **Unreleased** in
`CHANGELOG.md` as you go. The version bump is deliberately *not* part of these
commits.

### 2. Make the release commit

```sh
./scripts/bump.sh 0.7.1
git commit -am "release v0.7.1" && git push
```

`bump.sh` does four things, and refuses to run if **Unreleased** in
`CHANGELOG.md` is empty:

- sets `version` in `Cargo.toml`
- runs `cargo check`, which rewrites `Cargo.lock`'s own version line (skip
  this and the release build rewrites the lock afterwards, leaving a stray
  second commit; pinned dependency versions are untouched)
- renames the **Unreleased** heading in `CHANGELOG.md` to the version and
  today's date, and starts a fresh empty **Unreleased** above it
- updates the example version in this file

Commit everything it touched as one commit.

Two reasons this is one commit of its own:

- The binary must correspond to exactly one commit, so the tag you create in
  step 3 points at precisely what you shipped. `dmg.sh` refuses to build from a
  dirty tree for this reason (`ALLOW_DIRTY=1` overrides for local experiments).
- Installed copies decide whether to offer an update by comparing the release
  tag against the version compiled into the binary. A release that reuses the
  old version number is invisible to the updater.

### 3. Build and publish

```sh
./scripts/release.sh
```

This runs `dmg.sh` (sign, notarize, staple), then:

1. signs the update DMG with the minisign key (`~/.config/oxide-release/update.key`,
   or `$UPDATE_KEY`), once per architecture in the binary;
2. uploads the DMGs and signatures to the `oxide-releases` R2 bucket under
   `oxide/<version>/`, and checks they serve from
   `https://downloads.oxideterminal.com/oxide/<version>/`;
3. creates the GitHub release with both DMGs, with the `## [<version>]`
   section of `CHANGELOG.md` as the notes;
4. writes the update manifest — `releases/stable.json` in the bucket, plus a
   `releases/<version>.json` copy for rollbacks;
5. refreshes `oxide/latest/Oxide.dmg`, the one object that changes in place:
   it's what the website's download links point at, so they never go stale;
6. bumps the Homebrew cask and regenerates the site changelog.

It refuses to run if the changelog section is missing or empty, so a
forgotten changelog rename fails before the slow build starts, and likewise
if `update.pub` is still the placeholder, minisign or the key is missing,
wrangler isn't logged in, or the bucket already has this version.

If it fails partway (an upload, GitHub, the cask), fix the cause and rerun
with `SKIP_BUILD=1 ./scripts/release.sh`: it reuses the DMG in `target/`,
skips objects that are already in the bucket with the same size, and leaves
an existing GitHub release alone, so it picks up where it stopped. The DMG
goes up under two names:

- `Oxide-<version>.dmg` — what the website's download button and the cask serve
- `Oxide-<version>-update.dmg` — the same bytes, what the in-app updater fetches

Both R2 and GitHub count downloads per object, so the fresh-install count stays
apart from updates.

The order matters: the manifest is written last, after the DMG it points at
is confirmed reachable, so no installed copy ever sees an update it can't
fetch. GitHub stays a mirror: copies older than the manifest switch still
look there, and the release page is what the Linux pill opens.

The tag lands on current HEAD — the commit you just built from, because of
step 2. The script refuses to run if the tag already exists.

Notes on the DMG build:

- Notary credentials come from the `oxide-notary` keychain profile
  (falls back to `APPLE_ID` / `APPLE_TEAM_ID` / `APPLE_PASSWORD` env vars).
  To (re)create the profile:

  ```sh
  xcrun notarytool store-credentials oxide-notary \
    --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_PASSWORD"
  ```

- Notarization normally clears in ~1–3 minutes. (An account's first-ever
  submission gets extended review and can take an hour.)

#### Doing it by hand

If the build succeeded but publishing failed, or you need `--target <sha>` to
tag a different commit (`gh` rejects abbreviated SHAs; pass the full hash):

```sh
cp target/Oxide-0.7.1.dmg target/Oxide-0.7.1-update.dmg
minisign -S -s ~/.config/oxide-release/update.key \
  -m target/Oxide-0.7.1-update.dmg -x target/Oxide-0.7.1-update.dmg.macos-aarch64.minisig \
  -t "oxide 0.7.1 macos-aarch64"
for f in Oxide-0.7.1.dmg Oxide-0.7.1-update.dmg Oxide-0.7.1-update.dmg.macos-aarch64.minisig; do
  npx wrangler r2 object put "oxide-releases/oxide/0.7.1/$f" --remote --file "target/$f" \
    --cache-control "public, max-age=31536000, immutable"
done
gh release create v0.7.1 target/Oxide-0.7.1-update.dmg target/Oxide-0.7.1.dmg \
  --title "Oxide v0.7.1" \
  --notes "$(sed -n '/^## \[0.7.1\]/,/^## \[/p' CHANGELOG.md | sed '1d;$d')"
```

Then the manifest. Its shape (the `signature` is the `.minisig` file's contents
as one JSON string; `jq -Rs . < file.minisig` produces it):

```json
{
  "version": "0.7.1",
  "pub_date": "2026-09-30T18:00:00Z",
  "notes_url": "https://oxideterminal.com/changelog/#v0.7.1",
  "release_url": "https://github.com/oxide-terminal/oxide/releases/tag/v0.7.1",
  "download_url": "https://downloads.oxideterminal.com/oxide/0.7.1/Oxide-0.7.1.dmg",
  "sha256": "<sha256 of the DMG>",
  "assets": {
    "macos-aarch64": {
      "url": "https://downloads.oxideterminal.com/oxide/0.7.1/Oxide-0.7.1-update.dmg",
      "size": 11705200,
      "sha256": "<sha256 of the DMG>",
      "signature": "untrusted comment: …\n…\ntrusted comment: oxide 0.7.1 macos-aarch64\n…\n"
    },
    "linux-x86_64": {
      "url": "https://downloads.oxideterminal.com/oxide/0.7.1/oxide-0.7.1-linux-x86_64.tar.gz",
      "size": 8000000,
      "sha256": "<sha256 of the tarball>",
      "signature": "…"
    }
  }
}
```

(`linux-x86_64` is added by `release-linux.sh`; `release_url` is what the
Linux pill opens; `size` is for the website's download button. Finally
`oxide/latest/Oxide.dmg` gets a copy of the DMG with
`--cache-control "public, max-age=300"`.)

The trusted comment must be exactly `oxide <version> macos-<arch>` — the
updater checks it after the signature, so a real signature can't be reused for
another version. Upload it as `releases/0.7.1.json` (immutable) and
`releases/stable.json` (`--cache-control "public, max-age=60"`).

### 4. The Linux build (on the Linux box)

GPUI can't be cross-compiled, so the Linux tarball is built on the Linux
machine and attached to the release `release.sh` just created. The tag
points at the release commit, which is main's HEAD, so a plain pull lands
on it:

```sh
ssh linux-box
cd ~/Code/oxide-app/oxide
git pull --ff-only && git fetch --tags    # HEAD is at (or just past) v0.7.1
./scripts/release-linux.sh
```

This runs `linux-package.sh`, then signs `oxide-<version>-linux-x86_64.tar.gz`
with the same minisign key, uploads it and its `.minisig` to the bucket
under `oxide/<version>/`, mirrors it onto the GitHub release, adds a
`linux-x86_64` entry to `releases/<version>.json` and `releases/stable.json`,
refreshes `oxide/latest/oxide-linux-x86_64.tar.gz` for the website's Linux
download link, and bumps `packaging/aur/oxide-terminal-bin/PKGBUILD` to the
new version, URL and checksum. It refuses to run if the release or the manifest doesn't
exist yet, if the bucket already has this tarball, or if anything that goes
into the build (`Cargo.*`, `src/`, `assets/`, `packaging/docker/`) changed
since the tag — commits after the release that only touch docs are fine.

The manifest step is last, after the tarball is confirmed reachable, so
installed Linux copies only start announcing a release they can get. If
`stable.json` has moved on to a newer version in the meantime (a hotfix
released from the Mac before this ran), only the per-version file is
updated and the script says so.

`linux-package.sh` does the release build inside the `packaging/docker`
container (Ubuntu 22.04), not on the host. A binary links against the glibc
of whatever built it and won't start anywhere older, and the Linux box runs
Arch, whose glibc is newer than everyone's — that was issue #2. The
container's glibc 2.35 covers Ubuntu 22.04, Debian 12, Fedora 36 and later,
and the script checks the built binary against that floor and refuses to
package one that needs more. Then strip, and the tarball with the `.desktop`
entry, icons and `install.sh`. The first build compiles every dependency
from scratch and takes a while; later ones reuse `target/docker/`. Commit
that bump:

```sh
git commit -am "aur: oxide-terminal-bin 0.7.1" && git push
```

The package isn't on the AUR yet, so that's the end of the Linux release.
Once there's an AUR account (see the setup list below), each release also
publishes it:

```sh
cd packaging/aur/oxide-terminal-bin && makepkg --printsrcinfo > .SRCINFO
cp PKGBUILD .SRCINFO ~/aur/oxide-terminal-bin/
cd ~/aur/oxide-terminal-bin && git add -A && git commit -m "v0.7.1" && git push
```

Before the first push, `makepkg -si` in `packaging/aur/oxide-terminal-bin`
is the local check that the PKGBUILD fetches and installs from
downloads.oxideterminal.com.

Installed Linux copies look for the manifest's `linux-x86_64` entry, so the
update pill only appears once this step is done. The tarball is signed for
the record (the AUR package pins the sha256 instead); nothing on Linux
installs in place, so nothing verifies it yet.

One-time setup on the Linux box:

- `gh auth login` (the GitHub mirror goes through `gh`).
- `pacman -S minisign jq`, and a copy of the signing key at
  `~/.config/oxide-release/update.key` (or `UPDATE_KEY=…`). It's the same key
  as on the Mac — copy it over `scp`, never through a repo.
- Node, then `npx wrangler login` (the browser step works over SSH by pasting
  the URL it prints). Set `CLOUDFLARE_ACCOUNT_ID` if `npx wrangler whoami`
  lists more than one account.
- Docker, for the build container. The daemon isn't enabled at boot on the
  Arch box, so `sudo systemctl start docker` before a release. Run the
  script as yourself, never under `sudo` (the build runs as your uid so the
  output stays yours); if your user isn't in the `docker` group,
  `DOCKER="sudo docker" ./scripts/release-linux.sh`. The group is
  root-equivalent, so `sudo` for an occasional release is the safer habit.
- Not yet done — an [AUR account](https://aur.archlinux.org/register) with
  your SSH public key added, then the package clone; the first push creates
  the package:
  `git clone ssh://aur@aur.archlinux.org/oxide-terminal-bin.git ~/aur/oxide-terminal-bin`.
  Add `aur.archlinux.org` to `~/.ssh/known_hosts` on first contact.

## What happens after publishing

Installed macOS copies read
`https://downloads.oxideterminal.com/releases/stable.json` on launch and every
6 hours (or immediately via **Oxide → Check for Updates…**). They download the
DMG in the background, verify its minisign signature against the key compiled
into the app (`update.pub`), and only then show the top-right "click to
install" pill; on click they swap the bundle and relaunch. A download whose
signature doesn't check out is deleted and reported, never installed.

Installed Linux copies read the same manifest and, once it has a
`linux-x86_64` entry, show the pill; clicking it opens the release page
(`release_url` in the manifest) since nothing installs in place there.
Copies older than the manifest switch still read GitHub, which is one reason
the release is mirrored there.

Nothing else to do on the publishing side.

## Publishing to downloads.oxideterminal.com

One-time setup on the release machine. The bucket (`oxide-releases`, custom
domain `downloads.oxideterminal.com`) already exists in the Cloudflare account.

1. **Signing key.** Generate a minisign keypair and keep the secret key out of
   every repo:

   ```sh
   brew install minisign
   mkdir -p ~/.config/oxide-release
   minisign -G -p ~/.config/oxide-release/update.pub -s ~/.config/oxide-release/update.key
   cp ~/.config/oxide-release/update.pub update.pub    # in the oxide repo; commit it
   ```

   `minisign -G` asks for a password; `-S` asks for it again at each release.
   `-W` skips the password if you'd rather rely on the disk being encrypted.
   Back the `.key` file up somewhere that isn't this machine (a password
   manager works): if it's lost, every installed copy is stuck until it's
   updated by hand to a build with a new `update.pub`. Never commit it.

   `update.pub` in the repo is the placeholder until this is done. The app
   builds either way, but the updater refuses everything until the real key
   is in place.

2. **Wrangler.** The upload goes through Cloudflare's CLI:

   ```sh
   npx wrangler login
   npx wrangler whoami       # should list the account with the bucket
   ```

   With more than one Cloudflare account, set `CLOUDFLARE_ACCOUNT_ID` in the
   environment. `release.sh` invokes it as `npx --yes wrangler`, so there's
   nothing to install globally. `wrangler r2 object put` uploads in one
   request, which caps objects at a few hundred MB — far above the DMG.

3. **Caching.** Objects are uploaded with their `Cache-Control` set:
   versioned files a year and immutable, `stable.json` one minute, the
   `oxide/latest/` copies five minutes. Cloudflare's
   edge honours these on the custom domain, so nothing further is needed. If
   an update ever seems slow to appear, purge `releases/stable.json` from the
   dashboard (Caching → Configuration → Custom Purge).

4. **Cross-origin reads.** The homepage's download button reads
   `stable.json` from the browser to show the version, date and size, which
   needs a CORS policy on the bucket (R2 → `oxide-releases` → Settings → CORS
   policy). Without it the button still works; the version line just stays
   blank.

   ```json
   [
     {
       "AllowedOrigins": ["https://oxideterminal.com"],
       "AllowedMethods": ["GET", "HEAD"],
       "AllowedHeaders": ["*"],
       "MaxAgeSeconds": 3600
     }
   ]
   ```

5. **Try it** before a real release with `R2_BUCKET`/`DOWNLOADS_URL` pointed
   at a scratch bucket, or just check the pieces:

   ```sh
   npx wrangler r2 object put oxide-releases/test.txt --remote --pipe <<< hi
   curl -fsS https://downloads.oxideterminal.com/test.txt
   npx wrangler r2 object delete oxide-releases/test.txt --remote
   ```

**Rolling back** a bad release: put the previous version's manifest back as
`stable.json` —

```sh
npx wrangler r2 object get oxide-releases/releases/0.6.2.json --remote --file /tmp/m.json
npx wrangler r2 object put oxide-releases/releases/stable.json --remote --file /tmp/m.json \
  --content-type application/json --cache-control "public, max-age=60"
```

— and delete the bad GitHub release (`gh release delete v0.7.1 --yes`).
Installed copies never downgrade, so anyone who already got the bad build
needs the next release; ship it as a new version number.

## Gotchas

- **Never overwrite a published object.** `release.sh` refuses to run if the
  bucket already has this version. A fix goes out as a new version number;
  old versions stay so the cask's pinned URL and rollbacks keep working.
- **The tag must be `v<Cargo.toml version>`** (e.g. `v0.7.1` for `0.7.1`) and
  the GitHub release must have a `.dmg` asset, or updaters older than the
  manifest switch ignore it. They prefer `-update.dmg` and fall back to the
  plain one.
- **`update.pub` is part of the build.** Changing the signing key means a
  release signed with the *old* key that carries the new `update.pub`, then
  releases signed with the new one from there on. Installed copies only ever
  trust the key they were built with.
- Version numbering: bug fixes and polish get a patch bump (`0.1.1`), a new
  user-facing capability gets a minor bump (`0.2.0`).
- **Don't rebuild while a notarization is in flight** — `bundle.sh`/`dmg.sh`
  overwrite `target/Oxide.app`, and the submitted ticket only staples to the
  exact bytes that were uploaded.
- Development builds (`cargo run`) never auto-check for updates; only
  installed `.app` bundles do — and on Linux, release builds outside a
  `target/` directory.
