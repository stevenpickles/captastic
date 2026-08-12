# Chocolatey packaging

Captastic's Chocolatey package is self-contained: the release executables, licenses,
documentation, and example configuration are embedded in the `.nupkg`. Installation and
upgrades therefore do not download or execute a second installer. The package exposes both
`captastic.exe` and `captastic-desktop.exe` on `PATH` and adds the console-free desktop launcher
to the Start Menu.

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

Build the distribution binaries and portable archive from an elevated PowerShell prompt:

```powershell
cargo build --locked --profile dist --workspace
$archive = ./scripts/package-windows.ps1 -Version 0.1.0
./scripts/package-chocolatey.ps1 -Version 0.1.0 -ArchivePath $archive
```

Both generated artifacts are written beneath `dist/`, which is excluded from Git. Inspect and
test the package from that directory:

```powershell
choco install captastic --version 0.1.0 --source ./dist --yes
captastic doctor
Get-Command captastic, captastic-desktop
choco upgrade captastic --version 0.1.0 --source ./dist --yes --force
choco uninstall captastic --yes
```

Use a disposable Windows VM for the full install/upgrade/uninstall test before publishing. Check
that the Start Menu entry launches the tray application, the global hotkey captures successfully,
an upgrade stops the old daemon cleanly, and `~/.captastic` remains after uninstall.

## Release and publish

The release workflow builds the portable ZIP first, then embeds that exact archive's contents in
`captastic.<version>.nupkg`. Both packages and the ZIP checksum are uploaded as workflow artifacts;
tagged builds also attach them to the GitHub release. `VERIFICATION.txt` records the source archive
URL and SHA-256 hashes for the archive and both executables.

The workflow does not publish to the Chocolatey community repository automatically. Once a tagged
GitHub release and its artifacts have been checked, publish manually with an API key kept outside
the repository:

```powershell
choco push ./dist/captastic.0.1.0.nupkg `
    --source https://push.chocolatey.org/ `
    --api-key $env:CHOCOLATEY_API_KEY
```

The first package submission must pass Chocolatey community validation and moderation before
`choco install captastic` works against the default community source. Keep the package ID,
repository URLs, verification hashes, and release version aligned for each submission.
