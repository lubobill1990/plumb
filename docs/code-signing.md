# Code signing

Goal: the installer and `plumb.exe` on GitHub Releases carry a valid Authenticode
signature (Windows shows a verified publisher instead of "Unknown publisher" and
SmartScreen stops warning once the certificate has reputation).

## Choice: SignPath Foundation (free for open source)

[SignPath Foundation](https://signpath.org) gets code signing certificates (issued by a
CA in the Microsoft Trusted Root Program) in its own name and lets qualifying OSS
projects sign with them through [SignPath.io](https://signpath.io). Free of charge.

- The publisher shown by Windows is **SignPath Foundation**, not the author.
- The private key lives in SignPath's HSM. Builds must come from this repo's GitHub
  Actions (origin verification) and every release needs a **manual approval** in SignPath.
- Conditions: <https://signpath.org/terms>. They review project reputation: a very new
  project with little public use can be turned down ("reapply later").

Paid alternatives, if the application is declined or the publisher should be your own name:

| Option | Cost | Publisher | Notes |
|---|---|---|---|
| [Azure Artifact Signing](https://azure.microsoft.com/products/artifact-signing) (ex Trusted Signing) | $9.99/month, 5,000 signatures | your verified name / organization | individuals: US/Canada only; organizations: US, CA, EU, UK, AU, NZ, JP, KR, SG, CH, NO, IL; needs a paid (not MSDN/sponsored) subscription; signs synchronously, so Tauri's `signCommand` can sign the uninstaller too |
| Certum Open Source Code Signing | ~€49/year | "Open Source Developer, <your name>" | individuals worldwide; cloud key (SimplySign) |

## Status checklist

- [x] OSI license (MIT), no proprietary components, already released (v0.1.0), functionality documented in the README, uninstaller provided
- [x] "Code signing policy" section with roles and privacy statement on the home page (README)
- [x] Update check can be turned off (the only network access Plumb makes on its own)
- [x] Release workflow with SignPath steps (inactive until configured) — `.github/workflows/release.yml`
- [x] Artifact configurations with product name/version restrictions — `.signpath/artifact-configurations/`
- [ ] MFA on GitHub and SignPath for every team member (only @lubobill1990)
- [ ] Application submitted at <https://signpath.org/apply>
- [ ] After approval: SignPath configured (below)

## 1. Apply

Form: <https://signpath.org/apply>. Answers prepared:

- **Project name**: Plumb
- **Repository**: https://github.com/lubobill1990/plumb
- **Homepage / download page**: https://github.com/lubobill1990/plumb (README) / https://github.com/lubobill1990/plumb/releases
- **License**: MIT
- **Description**: A Windows terminal for AI coding agents (Claude Code, Copilot CLI, Codex …) built on Tauri and xterm.js. It fixes the causes of misaligned/garbled terminal UIs on Windows (ConPTY reflow, Unicode widths, IME input) and adds tabs, a launcher for shells and agent CLIs, find, and signed self-updates.
- **What gets signed**: `plumb.exe` and the NSIS installer `Plumb_<version>_x64-setup.exe`, built by GitHub Actions from tags.
- **Build system**: GitHub Actions (github.com), workflow `.github/workflows/release.yml`.
- **Team**: Bo Lu (@lubobill1990) — author, reviewer, approver.

## 2. After approval: configure SignPath and the repo

In SignPath (organization created for the project by SignPath Foundation):

1. **Trusted build system**: link GitHub.com to the project (origin verification for
   `lubobill1990/plumb`).
2. **Project** slug `plumb`.
3. **Artifact configurations** (paste the XML):
   - slug `exe` ← `.signpath/artifact-configurations/exe.xml`
   - slug `installer` ← `.signpath/artifact-configurations/installer.xml`
4. **Signing policy** slug `release-signing`: the Foundation certificate, approvers =
   you, origin verification on, allowed branch/tag refs `refs/tags/v*`.
5. **CI user** with submitter rights on that policy; create its API token.

In the GitHub repo (Settings → Secrets and variables → Actions):

```sh
gh secret   set SIGNPATH_API_TOKEN       -R lubobill1990/plumb   # the CI user's token
gh variable set SIGNPATH_ORGANIZATION_ID -R lubobill1990/plumb --body <org id>
# only if you chose other slugs:
gh variable set SIGNPATH_PROJECT_SLUG        -R lubobill1990/plumb --body plumb
gh variable set SIGNPATH_SIGNING_POLICY_SLUG -R lubobill1990/plumb --body release-signing
```

Setting `SIGNPATH_ORGANIZATION_ID` switches signing on; without it the release
workflow publishes unsigned builds.

Test without publishing: Actions → Release → Run workflow, tag `v<current version>`,
**dry run** on. It signs everything (two approvals) and keeps the files as an artifact.

## 3. Releasing (signed)

1. Bump the version, merge, push the tag `v<version>`.
2. SignPath emails you twice per release: approve **exe**, then **installer** (the
   workflow waits up to 5 hours for each).
3. The workflow checks both signatures, signs the final installer for the in-app
   updater, writes `latest.json` and publishes the release.

## How the pipeline works (and why)

1. `tauri build --no-bundle` → `plumb.exe`.
2. `tauri bundle` once. Tauri's bundler patches the bundle type into a **copy** of the exe,
   so signing `target/release/plumb.exe` before bundling would ship a broken signature.
   The workflow takes the patched exe out of that installer (7-Zip).
3. SignPath signs the patched exe.
4. `tauri bundle` again from the signed exe: the patch rewrites identical bytes, so the
   embedded `plumb.exe` keeps a valid signature (checked by the workflow, and verified
   locally with a test certificate).
5. SignPath signs the installer.
6. Only now the minisign signature for the in-app updater is made (`tauri signer sign`):
   Authenticode changes the installer's bytes, so the updater signature must come last.

Not signed: the uninstaller NSIS writes during installation (it is generated inside the
installer; signing it needs a synchronous `signCommand` during bundling, which SignPath's
approval flow can't provide; Azure Artifact Signing could).
