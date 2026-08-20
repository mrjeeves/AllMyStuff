# Release signing (minisign)

> **Maintainer runbook:** ordinary users and contributors do not need to
> configure release signing. See the [documentation map](docs/README.md) for
> install, usage, and development guides.

The self-updater (`allmystuff-updater`) verifies every downloaded artifact
before it stages it for the next launch:

1. **Integrity** — a published `<asset>.sha256` (or `SHA256SUMS`) is **mandatory**.
   A missing checksum now fails closed; nothing unverified is ever staged.
2. **Provenance** — when the shipped build has a release public key baked in
   (`ALLMYSTUFF_RELEASE_PUBKEY`), a valid detached **minisign** signature
   (`<asset>.minisig`) over the artifact is **required** before it is staged.

Until you complete the one-time setup below, releases keep working exactly as
before (SHA-256-only); the signing CI job is a no-op and the client logs that
signing isn't configured. This mirrors the scheme already used by the bundled
`myownmesh` daemon, so both halves of an install share one signing model.

> **Empty key == unconfigured.** The `bundles` job exports
> `ALLMYSTUFF_RELEASE_PUBKEY` unconditionally, so when the repo *variable* is
> unset the build still sees it as an empty string and `option_env!` yields
> `Some("")`, not `None`. The updater normalises an empty baked-in key back to
> "not configured" so an unconfigured repo really does degrade to SHA-256-only
> rather than demanding signatures it never publishes (which fails every update
> closed). Configuring the variable with a real key is what flips signing on.

## One-time setup

1. **Generate a password-less signing key** (CI must sign non-interactively):

   ```sh
   minisign -G -W -p minisign.pub -s minisign.key
   ```

   - `minisign.pub` holds a comment line and the base64 **public key** (line 2).
   - `minisign.key` is the **secret key** — treat it like any signing secret.

2. **Add the secret key to GitHub Actions** as repository secret
   `MINISIGN_SECRET_KEY` (the full contents of `minisign.key`). The `sign` job in
   `.github/workflows/release.yml` keys off this secret.

3. **Bake the public key into the shipped binaries.** Set repository *variable*
   `ALLMYSTUFF_RELEASE_PUBKEY` to the base64 public-key string (line 2 of
   `minisign.pub`, e.g. `sed -n '2p' minisign.pub`). The `bundles` job already
   passes it into the build env, so once set, `RELEASE_PUBKEY` in
   `crates/allmystuff-updater/src/lib.rs` is `Some(...)` and the client refuses
   any artifact without a valid signature. A repo *variable* is fine — the
   public key isn't secret.

   > The updater normalises this value, so pasting the **whole** two-line
   > `minisign.pub` (the `untrusted comment:` header included), or leaving a
   > trailing newline / CRLF, still works — it trims whitespace and, when the
   > value spans two lines, takes the second (the key always follows the
   > header). The base64 itself is still strictly checked, so a genuinely wrong
   > key fails closed. Don't paste the *secret* key (`minisign.key`) here.

4. **Cut a test release** and confirm `.minisig` sidecars sit next to each
   `allmystuff-*.tar.gz` / `.zip`, and that `allmystuff update` on a build
   compiled with the pubkey accepts it.

## What gets signed

The three portable archives the updater consumes — `allmystuff`,
`allmystuff-gui`, and `allmystuff-serve` — as `allmystuff-*.{tar.gz,zip}`. The
OS installer bundles (`.deb` / `.dmg` / `.msi`) are signed/notarized by their
own platform toolchains and are out of scope for this key. The bundled
`myownmesh` sidecar is pinned via `.myownmesh-rev` and built here, so it ships
inside these signed archives rather than carrying a separate signature.

## Rotation

Generate a new key, update both `MINISIGN_SECRET_KEY` and
`ALLMYSTUFF_RELEASE_PUBKEY`, and roll across two releases (sign with the old key
while shipping the new pubkey) for a seamless hand-off.
