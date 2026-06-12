# Release signing (minisign)

The self-updater (`allmystuff-updater`) verifies every downloaded artifact:

1. **Integrity** — a published `<asset>.sha256` (or `SHA256SUMS`) is **mandatory**.
   A missing checksum now fails closed; nothing unverified is ever staged.
2. **Provenance** — when the shipped build has a release public key baked in
   (`ALLMYSTUFF_RELEASE_PUBKEY`), a valid detached **minisign** signature
   (`<asset>.minisig`) over the artifact is **required** before it is staged.

Until you complete the one-time setup below, releases keep working exactly as
before (SHA-256-only); the signing CI job is a no-op and the client logs that
signing isn't configured. Turn it on when you're ready.

## One-time setup

1. **Generate a password-less signing key** (CI must sign non-interactively):

   ```sh
   minisign -G -W -p minisign.pub -s minisign.key
   ```

   - `minisign.pub` contains a comment line and the base64 **public key** (the
     second line). Keep it; it is not secret.
   - `minisign.key` is the **secret key**. Treat it like any signing secret.

2. **Add the secret key to GitHub Actions** as repository secret
   `MINISIGN_SECRET_KEY` (paste the entire contents of `minisign.key`).
   The `sign` job in `.github/workflows/release.yml` keys off this secret.

3. **Bake the public key into the shipped binaries.** Set
   `ALLMYSTUFF_RELEASE_PUBKEY` to the base64 public-key string (the second line
   of `minisign.pub`, no comment) in the build environment of the **Build CLI**
   and **tauri-action** steps of the release workflow, e.g.:

   ```yaml
   env:
     ALLMYSTUFF_RELEASE_PUBKEY: ${{ vars.ALLMYSTUFF_RELEASE_PUBKEY }}
   ```

   (A repo *variable* is fine — the public key isn't secret.) Once set,
   `RELEASE_PUBKEY` in `crates/allmystuff-updater/src/lib.rs` is `Some(...)` and
   the client refuses any artifact without a valid signature.

4. **Cut a test release** and confirm: the release has `.minisig` sidecars next
   to each `allmystuff-*.tar.gz` / `.zip`, and `allmystuff update` on a build
   compiled with the pubkey accepts it. Flip the order (pubkey before secret, or
   vice-versa) only briefly — a build that requires signatures against a release
   that has none will (correctly) refuse to update.

## Rotation

Generate a new key, update both `MINISIGN_SECRET_KEY` and
`ALLMYSTUFF_RELEASE_PUBKEY`, and cut a release. Clients still on the old pubkey
won't accept the new release until they update through one release signed by the
*old* key — so rotate across two releases (sign with old, ship new pubkey) if
you need a seamless hand-off.
