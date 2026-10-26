# Running this fork as the main repository

The source now names `8-six/spotifast` for releases, updates, packaging and
documentation. A few repository-owned resources cannot be inherited from
upstream and must be configured once, in GitHub settings:

1. **Release signing.** `assets/update-public-key.hex` is still upstream's
   public key. Release builds refuse to publish a `checksums.txt` that the
   committed key cannot verify, and the updater refuses any release without a
   valid signature. Generate a key with `native-packages sign-checksums --help`
   (or `native-packages keygen` when available), replace
   `assets/update-public-key.hex` with the new public key, and store the private
   key as the `SPOTIFAST_UPDATE_SIGNING_KEY` secret in a `release-signing`
   environment. Until then, tagging builds artifacts but the publish job fails
   on purpose.
2. **GitHub Pages.** Enable Pages with "GitHub Actions" as the source for the
   docs workflow. `docs/_config.yml` already points at `8-six/spotifast`.
3. **Packaging destinations.** AUR and Homebrew publishing stay off unless
   `PUBLISH_AUR`, `PUBLISH_HOMEBREW` and their secrets are configured. The
   upstream Flathub app ID (`rocks.spotifast.Spotifast`) is kept so existing
   installations upgrade in place, but the fork does not submit to Flathub.
4. **Triage automation.** `board.yml`, `issue-assessment.yml` and
   `triage-open-backlog.yml` run only when the repository owner is `crmne`,
   because they use upstream's Copilot Triage workflows and secrets.
5. **macOS and Windows signing** remain optional: the macOS job falls back to
   an unsigned bundle, and Windows installers build without a certificate.

Dependencies still come from `crmne/` repositories, as the repository policy
requires. The fork inherits them unchanged.
