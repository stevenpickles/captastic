# Chocolatey packaging

Captastic's Chocolatey package is self-contained: the release executables, licenses,
documentation, and example configuration are embedded in the `.nupkg`. Installation and
upgrades therefore do not download or execute a second installer. The package exposes
`captastic.exe` on `PATH` through a GUI shim, so the shim holds no console open while Captastic
runs, and adds it to the all-users Start Menu (`CommonPrograms`); the portable installer's shortcut
is per-user instead. `captastic-cli.exe`, the command line that asks the running Captastic for
captures ([ADR 0014](adr/0014-command-line-capture.md)), gets a console shim instead, so a
terminal waits for it and sees its output and exit code. The developer command line,
`captastic-dev.exe`, is not packaged. The all-users shortcut is a Chocolatey packaging convention only — the installed startup
entry, configuration, and daemon control remain scoped to the installing user's session.

The package intentionally does not launch Captastic on its first installation. Start it from
the Start Menu, then turn on **Start with Windows** in the tray menu if it should start with
Windows. When upgrading or uninstalling a running installation, the Chocolatey before-modify hook
stops the daemon before touching its files, by signalling the session control event
`Local\CaptasticDaemonControl-v1` and waiting for the daemon to release it. That needs no Captastic
executable, so the same hook stops a 0.2 daemon and a 0.3 one. The hook also leaves a marker
in `%TEMP%` when it stopped a running daemon, and the new version's install reads it and starts
Captastic again through `explorer.exe`, which hands the start to the signed-in user's desktop shell,
so Captastic comes back at that user's normal integrity level. The package never starts an elevated
tray process. A first install never launches, an upgrade of a Captastic that was not running does
not either, and an upgrade from 0.2 does not relaunch because 0.2's hook predates the marker; start
Captastic from the Start Menu in those cases. Uninstall
removes the command shim, the login entry and the shortcut but preserves configuration and logs
under `~/.captastic`.

Upgrading from 0.2 or earlier also retires `captastic-desktop.exe`, the console-free launcher those
versions shipped: the install removes its shim and repoints a login entry that names it at
`captastic.exe`. `captastic.exe` repeats that repair when it starts, for an entry the elevated
install could not see.

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

Formal releases follow one branch model: a `release/v<version>` branch is cut from `dev`, merged
to `main` by pull request, the `v<version>` tag is created on `main`, and the release branch is
then merged back to `dev` by pull request. The revision count above is measured from the newest
`v*` tag reachable from `HEAD`, and the tag sits on a merge commit only `main` contains, so `dev`
builds keep counting from the previous reachable tag or the repository root; open the back-merge
pull request from `main` instead if that ever matters.

The workspace version represents the next formal release. Advance it on `dev` immediately after the
release branch is merged back—for example, move from `0.1.0` to the intended `0.2.0` or `1.0.0` line—so subsequent `dev` and
`ci` identifiers are prereleases of the correct future version. `captastic-dev --version`,
`captastic-dev version --json`, `captastic.exe --version` (shown in a dialog), and the Windows
executable properties report the identity actually embedded in a
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
(Get-Command captastic).Source
(Get-Item "$env:ChocolateyInstall\lib\captastic\tools\captastic\captastic.exe").VersionInfo.ProductVersion
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

Use a disposable interactive Windows VM for the final release check. Hosted CI verifies silent
install, non-launching first install, shims, shortcut creation, upgrade, uninstall cleanup, and a
preserved settings fixture, but it has no interactive desktop for the capture workflow. The VM
covers what only a desktop can, on a machine that has never seen Captastic. Use the release's own
artifacts, downloaded from the GitHub release once it exists (or from the release workflow's run
before then). The upgrade steps also need an *earlier* package of the same release line to upgrade
from, since a prerelease sorts below its release: build one from the release commit with
`-PrereleaseLabel vm.1` and a `-SourceUrl` naming that commit (for example `0.3.0-vm.1`), which
carries the same relaunch hook.

**Disposable-VM checklist.** Take a snapshot first; restore it between the two halves.

Portable package, as a standard user:

1. Download the ZIP and its `.sha256`, check the hash, `Unblock-File` the ZIP, extract it, and run
   `.\install.ps1`. Captastic appears in the notification area with no console window, and its Start
   Menu shortcut exists. `.\captastic-cli.exe status` in the install folder prints
   `"running":true` and the daemon's version.
2. Press the hotkey, choose a region, and press Enter: the clipboard holds the region.
3. Confirm with **Pin** (Shift+Enter) and with **Edit** (Ctrl+Enter), then press **Done**: a pin
   appears, and the edited image reaches the clipboard.
4. Turn on **Start with Windows**, sign out and back in: Captastic starts with no console.
5. Run the installed `uninstall.ps1`: Captastic stops, the shortcut and login entry are gone, and
   `~/.captastic` remains.

Chocolatey package, from an elevated prompt, after restoring the snapshot:

6. `choco install captastic --version <version>-vm.1 --pre --source <folder>`: nothing launches.
   Start it from the Start Menu; there is no console, and `(Get-Command captastic).Source` is the
   Chocolatey shim. In a normal (not elevated) prompt, `captastic-cli capture --display primary`
   prints the path of a new capture with exit code 0 and raises *Captured by a command-line
   request*; `captastic-cli capture --window-title x` prints a refusal with exit code 2.
   Quit Captastic from the tray and run the same capture again: it prints `"taken_by":"one_shot"`
   with exit code 0 straight away, and the same notification appears from an icon that goes away
   about ten seconds later.
7. Repeat steps 2 and 3. Then, with a video or any sound playing, choose **Options -> Record As:
   MP4** and **Record Sound**, record a few seconds: the bar shows a speaker beside the time, and
   the review plays the sound with **Play** and silences it with **Mute**. Saved, the MP4 plays with
   sound in another player. (A VM with no audio device records silence, and the notification says
   it was recorded without sound: that is the expected result there.)
8. With Captastic running, `choco upgrade captastic --version <version> --source <folder>` to the
   release package: it stops, and comes back on its own within a few seconds. Task Manager's
   **Details** tab, with the **Elevated** column shown, reports `captastic.exe` as **No**, and the
   log's `starting daemon version=` line names the release.
9. `choco uninstall captastic`: Captastic stops, both shims, the shortcut and login entry are gone, and
   `~/.captastic` remains.

Record the result in the release notes' "What was not observed" section before publishing.

## Release and publish

The release workflow checks out complete tag history, reports the embedded build identity, invokes
`build-packages.ps1`, builds the portable ZIP first, then embeds that
exact archive's contents in `captastic.<version>.nupkg`. The ZIP, checksum, Chocolatey package, and
`artifacts.json` are uploaded explicitly; tagged builds also attach them to the GitHub release.
`VERIFICATION.txt` records the direct tagged GitHub release archive URL and SHA-256 hashes for the
archive and both executables. A tagged package build fails if given an indirect or non-release URL.

### Pushing to the Chocolatey community repository

For a release tag (`vX.Y.Z`, no prerelease label), the release workflow's **Push to the Chocolatey
community repository** job runs after the GitHub release is published. It waits on the
`chocolatey` environment, which requires the maintainer's approval. Approve it only once the
disposable-VM checklist has passed for this build and the published release looks right.

The job runs [scripts/publish-chocolatey.ps1](../scripts/publish-chocolatey.ps1), which builds
nothing:

1. It downloads the release's `artifacts.json` and every file it lists, through the direct release
   URLs that `VERIFICATION.txt` names and `choco install` fetches from.
2. It checks that the manifest is publishable and for the tag's version, and that each file has
   the SHA-256 the manifest records.
3. It pushes the `.nupkg` attached to the release, with the key from the environment's
   `CHOCOLATEY_API_KEY` secret.

One-time setup, in the repository's **Settings → Environments**:

1. Create an environment named `chocolatey`.
2. Add yourself under **Required reviewers**.
3. Limit its **Deployment branches and tags** to tags matching `v*`.
4. Add the secret `CHOCOLATEY_API_KEY`, holding the API key from your chocolatey.org account.

Without required reviewers the job pushes as soon as the release is published. Without the secret
it fails and pushes nothing.

To check a published release without pushing, or to push by hand if the job cannot run:

```powershell
./scripts/publish-chocolatey.ps1 -Tag v<version> -Repository stevenpickles/captastic -DryRun
$env:CHOCOLATEY_API_KEY = '<key>'   # then without -DryRun, to push
./scripts/publish-chocolatey.ps1 -Tag v<version> -Repository stevenpickles/captastic
```

The first package submission must pass Chocolatey community validation and moderation before
`choco install captastic` works against the default community source. Keep the package ID,
repository URLs, verification hashes, and release version aligned for each submission.
