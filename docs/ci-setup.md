# CI setup

One-time setup for the [`release` workflow](../.github/workflows/release.yml).
After that, releasing is just [pushing a tag](releasing.md).

**The secret names match the `yabai-plus` repo on purpose.** Both repos use the
same Apple account and the same tap org, so one set of values works for both.
If you have already set them up there, copy them across and skip to
[Verify](#verify).

## Signing modes

The **`MACOS_SIGNING`** repository variable picks how a release is signed. Set
it under **Settings → Secrets and variables → Actions → Variables**, not under
Secrets.

| `MACOS_SIGNING` | signature | needs | Accessibility after `brew upgrade` |
| --- | --- | --- | --- |
| unset, or `adhoc` | ad-hoc, no certificate | nothing | must be granted again, every release |
| `signed` | your certificate, not notarized | `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY` | kept |
| `notarized` | Developer ID, notarized | the above with a Developer ID, plus `APPLE_API_KEY`, `APPLE_API_ISSUER`, `APPLE_API_PRIVATE_KEY` | kept |

```sh
gh variable set MACOS_SIGNING --body signed     # or notarized
gh variable delete MACOS_SIGNING                # back to adhoc
```

**There is no paid Apple Developer account right now**, so the variable is
`signed`, and the certificate secrets hold the free account's Apple Development
certificate (`2RTC9978WY`). The App Store Connect secrets left over from the
paid team (8UR4G77744, now revoked) are not read in that mode. When there is a
paid account again, update the secrets (sections 1 and 2) and set the variable
to `notarized`. The workflow needs no edit.

Why the modes differ in Accessibility: macOS records the grant against a
binary's *designated requirement*. An ad-hoc signature has no identity, so its
requirement is the binary's own hash, which changes in every release. A
certificate gives a requirement that names the certificate rather than the
build, and that holds from one release to the next. Any real certificate gives
that, including a free account's Apple Development certificate.

Notarization only matters to Gatekeeper, and Gatekeeper checks only quarantined
files. Homebrew does not quarantine what a formula installs. So for anyone on
`brew install`, `signed` and `notarized` behave the same, and only a tarball
downloaded by hand in a browser tells them apart.

## 1. Signing certificate (`signed`, `notarized`)

For **`signed`**, a free Apple ID is enough:

1. In Xcode → Settings → Accounts, add the Apple ID, then **Manage
   Certificates** → **+** → **Apple Development**. `just` already signs local
   builds with this certificate (the justfile's `identity`).
2. In Keychain Access → **My Certificates**, select `Apple Development: …` *and
   its private key*. Right-click → Export, and set an export password.
3. The identity string is `security find-identity -v -p codesigning` without the
   hash, e.g. `Apple Development: eric.wang401@gmail.com (2RTC9978WY)`.

The certificate is valid for a year. When it expires, make a new one and update
the three secrets. Releases already shipped stay valid, because their
signatures carry a secure timestamp.

Signing CI releases with the same certificate `just` uses has a side effect. A
`brew upgrade` and a `just dev` build then share a designated requirement:
both are identified as `com.performave.rift-plus`, from the embedded
`assets/Info.plist`. So the Accessibility grant carries between them.

For **`notarized`**, you need an **Apple Developer Program** membership. Use a
**Developer ID Application** certificate instead, exported the same way. Its
identity looks like `Developer ID Application: Eric Wang (TEAMID)`.

## 2. App Store Connect API key (`notarized` only)

Used for notarization. An API key avoids the 2FA and session flakiness of
Apple-ID plus app-specific passwords.

1. <https://appstoreconnect.apple.com> → **Users and Access** → **Integrations**
   → **App Store Connect API** → generate a key with the **Developer** role.
2. Record the **Key ID** and **Issuer ID**.
3. Download `AuthKey_XXXXXXXX.p8`. **It can only be downloaded once.**

## 3. Secrets

Under **Settings → Secrets and variables → Actions → Secrets**:

| Secret | Mode | Value |
| --- | --- | --- |
| `APPLE_CERTIFICATE` | `signed`, `notarized` | base64 of the `.p12`: `base64 -i cert.p12 \| pbcopy` |
| `APPLE_CERTIFICATE_PASSWORD` | `signed`, `notarized` | the password set when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | `signed`, `notarized` | the full identity from step 1 |
| `APPLE_API_KEY` | `notarized` | App Store Connect **Key ID** |
| `APPLE_API_ISSUER` | `notarized` | App Store Connect **Issuer ID** (a UUID) |
| `APPLE_API_PRIVATE_KEY` | `notarized` | the raw contents of `AuthKey_XXXX.p8`, including the BEGIN/END lines |
| `HOMEBREW_TAP_TOKEN` | all | a token with `contents: write` on `performave/homebrew-tap` |

To set them with `gh` instead of clicking through the settings page:

```sh
gh secret set APPLE_CERTIFICATE          < <(base64 -i cert.p12)
gh secret set APPLE_CERTIFICATE_PASSWORD --body 'the-p12-password'
gh secret set APPLE_SIGNING_IDENTITY     --body 'Apple Development: eric.wang401@gmail.com (2RTC9978WY)'
gh secret set APPLE_API_KEY              --body 'ABC123DEF4'
gh secret set APPLE_API_ISSUER           --body '00000000-0000-0000-0000-000000000000'
gh secret set APPLE_API_PRIVATE_KEY      < AuthKey_ABC123DEF4.p8
gh secret set HOMEBREW_TAP_TOKEN         --body 'ghp_...'
```

Without `HOMEBREW_TAP_TOKEN` the release still builds and publishes. Only the
formula bump is skipped, with a warning saying so.

## Why these choices

These mirror a setup already known to work, and sidestep the usual notarization
failures:

- **A variable, not secret presence, picks the mode.** Secrets left over from a
  lapsed account are still "set", so their presence says nothing about whether
  they work. `codesign` will sign with a revoked certificate without complaint.
- **`apple-actions/import-codesign-certs`** for the import. It creates a
  temporary keychain and sets the key partition list, so `codesign` works
  without prompting. Doing that by hand is the usual failure on recent runners.
- **An App Store Connect API key** rather than Apple-ID auth — no 2FA in CI.
- **The `.p8` stored as raw text**, written with `printf`, so no base64
  round-trip can mangle it.

## Verify

1. `gh variable list` shows `MACOS_SIGNING`, or nothing for `adhoc`.
   `gh secret list` shows the secrets that mode needs.
2. Trigger a run: push a `v*` tag, or use **workflow_dispatch** on the Actions
   tab with a tag that already exists.
3. **Choose signing mode** prints the mode. The run summary repeats it, and the
   **Codesign** step prints the `Authority=` lines it signed with. An ad-hoc
   signature has none.
4. Under `notarized`, watch **Notarize**. `notarytool ... --wait` blocks until
   Apple answers `Accepted`, or rejects with a log URL.

## Troubleshooting

| Symptom | Likely cause |
| --- | --- |
| "MACOS_SIGNING is '…'" | the variable holds something other than `adhoc`, `signed` or `notarized` |
| "needs the APPLE_SIGNING_IDENTITY secret" | `signed`/`notarized` set without the certificate secrets |
| cert import fails | bad `APPLE_CERTIFICATE` base64 (re-export, use `base64 -i`) or wrong `APPLE_CERTIFICATE_PASSWORD` |
| `codesign`: no identity found | `APPLE_SIGNING_IDENTITY` does not match `security find-identity` output exactly |
| `codesign --verify`: `CSSMERR_TP_CERT_REVOKED` | the certificate was revoked, as the lapsed paid team's was. `codesign` still signs with it, but macOS kills the binary at launch; switch to `adhoc` or a valid certificate |
| notarytool `Invalid` | not signed with `--options runtime` / `--timestamp`, or signed with a non-Developer-ID cert |
| notarytool auth error | wrong Key ID or Issuer ID, truncated `.p8` text, or no active Apple Developer Program membership |
| release not created | tag did not match `v*`, or `contents: write` is missing |
| formula not bumped | `HOMEBREW_TAP_TOKEN` missing or lacking write access to the tap |
