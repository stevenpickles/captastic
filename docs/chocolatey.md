# Chocolatey packaging

Captastic's Chocolatey package is self-contained: the release executables, licenses,
documentation, and example configuration are embedded in the `.nupkg`. Installation and
upgrades therefore do not download or execute a second installer. The package exposes both
`captastic.exe` and `captastic-desktop.exe` on `PATH` and adds the console-free desktop launcher
to the all-users Start Menu (`CommonPrograms`); the portable installer's shortcut is per-user
instead. The all-users shortcut is a Chocolatey packaging convention only — the installed startup
entry, configuration, and daemon control remain scoped to the installing user's session.

The package intentionally does not launch Captastic on its first installation. Start it from
the Start Menu, then use the tray menu or `captastic startup enable` if it should start with
Windows. When upgrading a running installation, the Chocolatey hook stops the daemon before
replacing its files; reopen Captastic from the Start Menu after the upgrade. The package never
starts an elevated tray process from Chocolatey. Uninstall removes both command shims and the shortcut
but preserves configuration and logs under `~/.captastic`.

The Chocolatey package supports installation, upgrade, and removal by the same interactive
Windows user. Deployment as `SYSTEM`, elevation with a different administrator account, and
multi-user/fast-user-switching migration are not currently supported: Captastic's startup entry,
configuration, and daemon control signal are deliberately scoped to one user session. Managed
deployment tooling should stop Captastic in each affected user session before modifying files.

If the current user installed Captastic previously with the portable package's `install.ps1`,
the Chocolatey install invokes that installation's uninstaller first. This prevents the older
daemon from retaining the session control event; the migration also preserves `~/.captastic`.

## Build and test locally

Run the canonical packaging command from an elevated PowerShell 7 (`pwsh`) prompt:

```powershell
./scripts/build-packages.ps1
```

The command resolves the package containing the `captastic` Cargo binary, uses its release version,
builds the `dist` Cargo profile, and creates the portable ZIP, its SHA-256 file, the Chocolatey
package, and `artifacts.json` beneath `dist/`. Schema 2 of the manifest distinguishes the package
version from the version embedded in the executables and records the release core, build channel,
full Git commit, revision count, source tag, dirty state, CI run provenance, Rust target,
architecture, Chocolatey CLI version, source archive URL, filenames, and artifact hashes. Local,
CI, and release packaging use this same command.

Use a label such as `-PrereleaseLabel ci.123.1.gabcdef123` for an incremental package whose release
core must still match the Cargo version. GitHub Actions creates this label from the workflow run,
attempt, and commit. `-ReleaseTag v0.1.0` additionally proves that the matching tag points at `HEAD`,
the source is clean, the Cargo version matches, and both executables embed the exact formal version;
only then is the manifest marked publishable. `-BinariesDirectory` with `-SkipBuild` consumes
binaries already built by CI, but rejects them when their embedded commit differs from the source
being packaged. Portable ARM64 archives can be built with `-Architecture arm64`, but the Chocolatey
package is intentionally restricted to x86_64 until its multi-architecture contract is defined.

Formal releases follow one branch model: a `release/v<version>` branch is cut from `dev`, `main`
is fast-forwarded to it, the `v<version>` tag is created on `main`, and the release branch is then
merged back to `dev`. Fast-forwarding matters because the revision count above is measured from the
newest `v*` tag reachable from `HEAD`; a tag on a `main`-only merge commit would never be reachable
from `dev`.

The workspace version represents the next formal release. Advance it on `dev` immediately after the
release branch is merged back—for example, move from `0.1.0` to the intended `0.2.0` or `1.0.0` line—so subsequent `dev` and
`ci` identifiers are prereleases of the correct future version. `captastic --version`, `captastic
version --json`, and the Windows executable properties report the identity actually embedded in a
package; `artifacts.json` records that as `buildVersion` even when a lifecycle test deliberately
assigns two package versions to the same binaries.

Run the isolated packaging suite before inspecting the artifacts:

```powershell
./scripts/test-packaging.ps1
$manifest = Get-Content ./dist/artifacts.json -Raw | ConvertFrom-Json
$manifest.artifacts
./scripts/test-artifact-manifest.ps1 -ManifestPath ./dist/artifacts.json
```

The generated artifacts are excluded from Git. Install and inspect the package from its output
directory:

```powershell
choco install captastic --version <version> --source ./dist --yes
captastic doctor
Get-Command captastic, captastic-desktop
choco upgrade captastic --version <version> --source ./dist --yes --force
choco uninstall captastic --yes
```

Substitute `<version>` with the package version reported in `dist/artifacts.json`.

The destructive lifecycle test used by CI requires two locally built prerelease versions and an
explicit opt-in because it changes the machine's installed Chocolatey state:

```powershell
./scripts/test-chocolatey-lifecycle.ps1 `
    -InitialManifestPath ./dist/lifecycle-initial/artifacts.json `
    -UpgradeManifestPath ./dist/lifecycle-upgrade/artifacts.json `
    -AllowSystemChanges
```

Use a disposable interactive Windows VM for the final release check. Confirm that the Start Menu
entry launches the tray application, the global hotkey captures successfully, an upgrade stops a
running daemon cleanly, and `~/.captastic` remains after uninstall. Hosted CI verifies silent
install, non-launching first install, shims, shortcut creation, upgrade, uninstall cleanup, and a
preserved settings fixture, but it has no interactive desktop for the capture workflow.

## Release and publish

The release workflow checks out complete tag history, reports the embedded build identity, invokes
`build-packages.ps1`, builds the portable ZIP first, then embeds that
exact archive's contents in `captastic.<version>.nupkg`. The ZIP, checksum, Chocolatey package, and
`artifacts.json` are uploaded explicitly; tagged builds also attach them to the GitHub release.
`VERIFICATION.txt` records the direct tagged GitHub release archive URL and SHA-256 hashes for the
archive and both executables. A tagged package build fails if given an indirect or non-release URL.

The workflow does not publish to the Chocolatey community repository automatically. Do not push a
package until the tagged GitHub release exists, its direct download URLs work, its manifest hashes
match, and the disposable-VM checklist passes. Then publish manually with an API key kept outside
the repository:

```powershell
choco push ./dist/captastic.<version>.nupkg `
    --source https://push.chocolatey.org/ `
    --api-key $env:CHOCOLATEY_API_KEY
```

The first package submission must pass Chocolatey community validation and moderation before
`choco install captastic` works against the default community source. Keep the package ID,
repository URLs, verification hashes, and release version aligned for each submission.
