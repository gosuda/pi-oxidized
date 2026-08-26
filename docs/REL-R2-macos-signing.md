# REL-R2 — macOS signing & notarization channel (research record)

Stable ID `REL-R2`, issue metaphorics/pi-oxidized#118. **Research only and non-gating:**
this document records what signed, notarized `x86_64-apple-darwin` and
`aarch64-apple-darwin` shipping would require and which pieces exist today. It introduces
no executable or release-config changes and does not modify the unsigned seven-target
release definition; that definition remains the ship contract, unchanged.

Authored 2026-08-26. Tool and channel facts below are grounded in primary sources current
as of this date. GitHub-hosted runner images rotate on a weekly cadence, so image-pinned
versions carry the snapshot they were read from.

---

## 1. Scope and the unsigned baseline this does not touch

Issue #118 asks for a dated evidence record covering:

- the **Developer ID Application certificate and Apple Developer Program membership**
  status — which credentials **exist** vs which **must be provisioned**;
- the **secrets topology** for the GitHub `macos-15[-intel]` runners the release already uses;
- the exact runner-side **hardened-runtime + `codesign` -> `notarytool submit --wait`
  -> `stapler` -> `spctl`** chain, with tool versions re-grounded from the release channel;
- a **go/no-go** record.

The current tree carries **zero signing configuration**. The darwin legs of
`.github/workflows/release-verification.yml` run on `macos-15-intel` (for
`x86_64-apple-darwin`) and `macos-15` (for `aarch64-apple-darwin`), and the release
assembly in `scripts/release/*` builds plain `tar.gz` archives with `.sha256` sidecars and
performs `codesign`-free, `notarytool`-free verification. No `codesign`, `notarytool`,
`stapler`, `spctl`, keychain, `.p12`, or `.p8` artifact appears anywhere in the workflow,
release scripts, or repository tree. The issue names this the "unsigned seven-target
definition"; the concrete assembly (`scripts/release/targets.ts`) currently enumerates five
Rust triples, of which two are darwin. Nothing in the two darwin legs signs or notarizes.

## 2. Tool and channel facts (as of 2026-08-26)

Primary source: the GitHub `actions/runner-images` image README and GitHub's hosted-runner
reference. The image snapshot current on 2026-08-26 is `20260727.0377.1`, macOS
**15.7.7 (24G720)**, Darwin 24.6.0. Both `macos-15-intel` (Intel/x86_64) and `macos-15`
(Apple Silicon/arm64) are the macOS 15 line and carry the same toolchain:

- **Xcode:** default `16.4` (build 16F6); `26.0.1`, `26.1.1`, `26.2`, `26.3` also installed.
- **Xcode Command Line Tools:** `16.4.0.0.1.1747106510`.
- **Signing stack:** `codesign`, `notarytool`, `stapler`, `spctl` (and `xcrun`) ship with
  the Xcode/CLT install and are present on both runner labels. Version of the `notarytool`
  client therefore tracks the Xcode/CLT on the image (16.4 default at this snapshot; newer
  Xcode 26.x may be selected via `xcode-select -s`).
- The image's Rust toolchain is **1.97.1** — identical to the release workflow's pinned
  toolchain, so the existing darwin build legs are already aligned.

Runner specification (GitHub hosted-runner reference): `macos-15-intel` = Intel, 4 vCPU,
14 GB RAM; `macos-15` = Apple Silicon (M1), 3 vCPU, 7 GB RAM. Images update weekly; exact
per-run software is listed under the "Set up job" → "Installed Software" log section.

## 3. Credentials inventory — exists vs must be provisioned

### Repository and workflow availability
The tree and the GitHub workflow reference no Apple credential, signing identity,
or signing secret.

### External account state
Repository absence cannot establish whether the organization already holds an
active Apple Developer Program membership, a Developer ID certificate and
private key, or an App Store Connect API key. An account owner must confirm
those facts outside this repository.

### Required credential set
Confirm or provision every item below before enabling the signed channel.


| Credential | Purpose | Who confirms or provisions | Notes |
|---|---|---|---|
| Apple Developer Program membership | Prerequisite for every Developer ID / notarization credential | Apple Developer account owner | Confirm active membership; enroll only if absent. |
| **Developer ID Application** certificate | Code-signs the darwin binaries with a stable identity Gatekeeper trusts for external distribution | Account Holder, under Certificates, Identifiers & Profiles (or Xcode → Settings → Manage Certificates) | Confirm an unexpired certificate or provision one. "Apple Development"/"Apple Distribution" do not replace this outside-the-Mac-App-Store identity. |
| Certificate private key + `.p12` export | Lets the runner import the identity into a temporary keychain for `codesign` | Certificate creator | Confirm the private key is recoverable; export it with a passphrase if no usable `.p12` exists. |
| **App Store Connect API key** (issuer ID, key ID, `.p8` private key) | Authenticates `notarytool` to Apple's notary service without a shared Apple ID password | Account Holder requests API access; Admin/Account Holder generates the team key | Confirm a usable key or create one. `.p8` is downloadable once. |
| GitHub Actions secrets (repo or org level) | Carries the credentials into the runner without hardcoding | Repo/organization admin | None are referenced today; add them only after the external inventory is confirmed. See §4. |

The signed path is blocked on credential confirmation and workflow secret
installation, not on runner tooling.

## 4. GitHub macOS runner secret topology

Secrets are repo- or organization-level **Actions secrets** (Settings → Secrets and
variables → Actions), referenced in workflows as `${{ secrets.NAME }}`. The canonical
pattern for Apple signing on GitHub-hosted macOS runners stores the certificate as
Base64-encoded `.p12` plus its passphrase, a throwaway runner keychain password, and the
notarization credentials as distinct secrets:

| Secret | Contents |
|---|---|
| `BUILD_CERTIFICATE_BASE64` | Developer ID `.p12` exported from Keychain Access, Base64-encoded |
| `P12_PASSWORD` | Passphrase protecting the `.p12` |
| `KEYCHAIN_PASSWORD` | Random password for the temporary runner keychain |
| `APPLE_ID` / `TEAM_ID` / notary API key (`ISSUER_ID` + `KEY_ID` + `KEY_P8_BASE64`) | Authentication for `notarytool` |

On the runner, the certificate is imported into a temporary keychain that is created,
unlocked, and (best practice) deleted at the end of the job. The two darwin legs
(`macos-15-intel`, `macos-15`) each need the same set of secrets; organization-level
secrets avoid per-repo duplication. Relevant topology note: Intel macOS runners carry a
static UDID (`4203018E-580F-C1B5-9525-B745CECA79EB`) while arm64 runners do not — this
matters for device-registered profiles, but **Developer ID signing and notarization do not
require UDID registration**, so it does not gate the darwin release path.

## 5. Exact runner-side chain (hardened runtime + timestamp -> notarytool -> staple -> verify)

Primary sources: Apple "Notarizing macOS software before distribution" and the
`notarytool` documentation. All steps run inside the GitHub-hosted darwin job after the
existing build/package step produces the darwin binary.

1. **Import the certificate** into a temporary keychain and grant unattended
   `codesign` access to the private key:

       security create-keychain -p "$KEYCHAIN_PASSWORD" build.keychain
       security unlock-keychain -p "$KEYCHAIN_PASSWORD" build.keychain
       security import certificate.p12 -k build.keychain \
         -P "$P12_PASSWORD" -A -t cert -f pkcs12
       security set-key-partition-list -S apple-tool:,apple: \
         -k "$KEYCHAIN_PASSWORD" build.keychain
       security list-keychains -d user -s build.keychain
2. **Code-sign each executable with hardened runtime and timestamp, then verify it:**

       codesign --force --options runtime --timestamp \
         --sign "Developer ID Application: <Your Name> (<TEAM_ID>)" \
         <binary>
       codesign --verify --strict --verbose=2 <binary>

   `--options runtime` enables the mandatory **Hardened Runtime**; `--timestamp` embeds an
   authentication timestamp so the signature remains valid after the cert or notary ticket
   ages. Hardened Runtime is a requirement for notarization. Do not use `--deep` as a
   substitute for signing nested code explicitly.
3. **Build a staple-capable submission container.** A ZIP can be notarized, but Apple does
   not support stapling a ticket to a ZIP or a bare executable. For the required
   submit/staple chain, place the signed binaries in a DMG:

       hdiutil create -volname pi-oxidized -srcfolder <staging-dir> \
         -ov -format UDZO pi-oxidized.dmg
4. **Submit and wait:**

       xcrun notarytool store-credentials <profile> \
         --key <AuthKey_<KEY_ID>.p8> --key-id <KEY_ID> --issuer <ISSUER_ID>
       xcrun notarytool submit pi-oxidized.dmg --keychain-profile <profile> --wait

   `--wait` blocks until Apple returns the submission's terminal status instead of
   requiring manual polling. On failure, `xcrun notarytool log <submission-id>` returns the
   JSON diagnostic. Apple's `notarytool` is the current required tool; the older
   `altool`/two-step first-/second-submit workflow is superseded.
5. **Staple the notarization ticket** so Gatekeeper can verify offline:

       xcrun stapler staple pi-oxidized.dmg
       xcrun stapler validate pi-oxidized.dmg

6. **Assess the stapled DMG, then mount it and assess each executable:**

       spctl -a -t open --context context:primary-signature \
         -v pi-oxidized.dmg
       hdiutil attach pi-oxidized.dmg
       spctl -a -vv --type exec <mounted-dmg-path>/<binary>

   The first command exercises Gatekeeper's disk-image assessment and the
   stapled ticket. The executable check separately verifies the Developer ID
   signature after mounting.

Notarization is an automated security scan, not App Review; ad-hoc or development
certificates do not qualify — only a valid Developer ID signature does.

## 6. Go / No-Go verdict

The verdict is **NO-GO** for shipping signed and notarized darwin archives as
of 2026-08-26. The signed path is not wired into the current workflow. External
Apple account credentials and GitHub Actions secret values are not observable
from this repository, so their availability remains unverified. The path stays
NO-GO until an account owner confirms the Apple credential inventory and a
repository administrator confirms or provisions the required Actions secrets.
The required tools are already present on the `macos-15-intel` and `macos-15`
runners at the dated image snapshot.

This verdict is **non-gating by design**: the unsigned seven-target release definition is
unaffected and remains the current ship contract. The darwin legs already build and package
correctly unsigned; signing/notarization is a credential-gated follow-on, as the issue
states.

## 7. Open items for the REL implementer (out of scope here)

- Archive/container strategy for notarization: `notarytool` accepts ZIP, DMG, and PKG, but
  tickets can be stapled only to supported staple targets such as DMG, PKG, and app
  bundles. The current release emits `tar.gz`, so a signed channel that requires offline
  ticket validation must add a staple-capable container. A ZIP-only channel would depend
  on Apple's online ticket lookup and would not satisfy the submit/staple chain above.
- Selecting the Xcode/CLT version per run (`sudo xcode-select -s`) to pin the
  `notarytool` client, since the image default shifts weekly.
- Key-rotation lifecycle for the App Store Connect API key and the Developer ID
  certificate (Developer ID certs last limited years and must be re-signed before expiry).

## 8. Primary sources

- Apple — [Notarizing macOS software before distribution](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
- Apple — [Customizing the notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow)
- Apple — [Hardened Runtime](https://developer.apple.com/documentation/security/hardened-runtime)
- Apple — [Migrating to the latest notarization tool (TN3147)](https://developer.apple.com/documentation/technotes/tn3147-migrating-to-the-latest-notarization-tool)
- Apple — `notarytool` [man page](https://keith.github.io/xcode-man-pages/notarytool.1.html)
- Apple — [Creating API keys for App Store Connect API](https://developer.apple.com/documentation/appstoreconnectapi/creating-api-keys-for-app-store-connect-api)
- GitHub — [Installing an Apple certificate on macOS runners for Xcode development](https://docs.github.com/actions/use-cases-and-examples/deploying/installing-an-apple-certificate-on-macos-runners-for-xcode-development)
- GitHub — [GitHub-hosted runners reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
- GitHub — [`actions/runner-images` — macOS 15 image README](https://github.com/actions/runner-images/blob/main/images/macos/macos-15-Readme.md)
- GitHub — [Using encrypted secrets in a workflow](https://docs.github.com/en/actions/security-for-github-actions/security-guides/using-secrets-in-github-actions)