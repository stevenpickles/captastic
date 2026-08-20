# Unsigned releases

Captastic releases are not Authenticode-signed. Nothing in a Captastic download carries a publisher
certificate, and Windows will say so. This document explains why that is the current state, what it
looks like on a workstation, and how to establish for yourself that the files you downloaded are the
files the release workflow produced.

## Why the releases are unsigned

Signing is deliberately sequenced after the capture milestones rather than omitted from them. The
roadmap's release signing and distribution backlog states the constraints directly: until signing is
provisioned, Captastic continues publishing deterministic packages with SHA-256 checksums and clear
unsigned-release documentation, does **not** store a long-lived PFX/private key in repository or
ordinary CI secrets, and keeps the release workflow structured so signing can be inserted after final
build and before packaging.

The reason for the middle constraint is the one that decides the ordering. A code-signing key held in
a repository or in ordinary CI secrets is a key that every workflow run, every fork, and every person
with secret access can use; a signature produced from it identifies the pipeline that held it rather
than the project. Shipping unsigned is honest about the absence of a publisher identity. Shipping
signed with a key kept that way would assert one that had not actually been protected.

When signing is provisioned, the backlog names what it has to be: a public-trust provider, HSM or
managed signing rather than a portable key file, an RFC 3161 SHA-256 timestamp so signatures outlive
the certificate, and a release that fails outright when its signatures do not verify. MSI/MSIX
packaging and automatic updates are to be evaluated only after signing identity and release channels
are stable, because both distribute trust decisions that a signature is what anchors.

## What Windows will show you

**SmartScreen on first run.** Running a downloaded, unsigned executable that Windows has no
reputation for produces the blue full-screen interstitial titled *Windows protected your PC*, with
*Don't run* as the only visible button. Choosing **More info** reveals the file name, an *Unknown
publisher* line, and a **Run anyway** button. This is the expected appearance for an unsigned release
and not a report that anything was found in the file. SmartScreen's reputation is accumulated per
file, so each new release version starts without it.

**Browser download warnings.** Browsers apply their own reputation checks to executable downloads and
may warn, ask for confirmation, or require the download to be kept explicitly. The same absence of a
publisher identity is what they are reacting to.

**Nothing in the tray or the daemon changes.** The SmartScreen prompt applies to launching the
downloaded file; it is not a runtime permission and Captastic does not request elevation.

## Why `Unblock-File` is needed

When a file is downloaded, Windows records its origin in an NTFS alternate data stream named
`Zone.Identifier` — the Mark of the Web. PowerShell refuses to run marked scripts under ordinary
execution policies, and Windows treats marked executables with extra suspicion.

The mark attaches to the ZIP archive, and **files extracted from a marked archive inherit the mark**.
Unblocking the archive before extracting it is therefore the shorter path, because it produces
unmarked files:

```powershell
Unblock-File .\captastic-<version>-windows-x86_64.zip
```

If the archive was already extracted while it was marked, clear the mark from the extracted files
instead, from inside the extracted directory:

```powershell
Get-ChildItem -Recurse | Unblock-File
```

`Unblock-File` removes the origin record. It does not vouch for the file, which is what the checksum
verification below is for.

## Verifying what you downloaded

A tagged release attaches four files:

| File | What it is |
| --- | --- |
| `captastic-<version>-windows-x86_64.zip` | The portable archive: both executables, the example configuration, the licenses, `install.ps1`, and `uninstall.ps1`. |
| `captastic-<version>-windows-x86_64.zip.sha256` | The archive's SHA-256 checksum, as a lowercase hash followed by the archive's file name. |
| `captastic.<version>.nupkg` | The self-contained Chocolatey package. |
| `artifacts.json` | The build manifest: package version, embedded build identity, source provenance, and a SHA-256 hash for each of the three files above. |

### Against the published checksum file

```powershell
$archive = '.\captastic-0.1.0-windows-x86_64.zip'
$expected = (Get-Content -LiteralPath "$archive.sha256" -Raw).Trim().Split()[0]
$actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash

if ($actual -ieq $expected) { "match: $archive" } else { "MISMATCH: $archive" }
```

`Get-FileHash` returns an uppercase hash and the published values are lowercase, so compare
case-insensitively with `-ieq` rather than `-eq`.

### Against the manifest

`artifacts.json` covers every attached file, including the checksum file itself, so one loop checks
the whole release. Run it in the directory the release files were downloaded to:

```powershell
$manifest = Get-Content -LiteralPath .\artifacts.json -Raw | ConvertFrom-Json

foreach ($artifact in $manifest.artifacts) {
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath ".\$($artifact.file)").Hash
    if ($actual -ieq $artifact.sha256) {
        "match    $($artifact.kind)  $($artifact.file)"
    } else {
        "MISMATCH $($artifact.kind)  $($artifact.file)"
    }
}
```

The manifest also records the exact Git commit, revision count, source tag, and build channel the
package was produced from, and `publishable` is true only for a build made from a matching clean tag.
`captastic version --json` reports the same identity from the installed executable, so a downloaded
package can be checked against what it claims to be after installation as well as before it.

Be clear about what this establishes and what it does not. Matching hashes prove that the bytes on
disk are the bytes the release workflow published — they detect truncation, a corrupted download, and
substitution of the file. They say nothing about who published the release, because the checksums are
served from the same GitHub release page as the files they describe. A signature is what would carry
publisher identity independently of the download location, and that is exactly the property these
releases do not yet have.

## The Chocolatey package

The Chocolatey package is self-contained: the release executables, licenses, documentation, and
example configuration are embedded in the `.nupkg`, so installing or upgrading downloads and executes
no second installer. The release workflow builds the portable ZIP first and then embeds that exact
archive's contents in the package, so the binaries a Chocolatey install places on disk are the ones
covered by the archive hash above.

`VERIFICATION.txt`, inside the package under `tools`, records the direct tagged GitHub release
archive URL, the archive's SHA-256 hash, and the SHA-256 hashes of `captastic.exe` and
`captastic-desktop.exe`, with instructions to download the archive, hash it with `Get-FileHash`,
extract it, and compare. That file exists for Chocolatey's moderators, who verify that an embedded
binary matches an upstream release before a package is approved, and it is equally usable by anyone
who has the package. A tagged package build fails if it is given an indirect or non-release URL.

Community publishing is manual and gated on that review. The package must pass Chocolatey community
validation and moderation before `choco install captastic` resolves against the default community
source. See [Chocolatey packaging](chocolatey.md) for the publishing procedure.

## When signing lands

Most of this document is a description of an absence, and it should get shorter. Once Authenticode
signing is provisioned, the SmartScreen and unknown-publisher sections describe behaviour that no
longer occurs, and checksum verification becomes a supplement to signature verification rather than
the only integrity evidence available. The Mark of the Web section survives — the mark is applied to
downloads regardless of signing — and so does the manifest, which records build provenance that a
signature does not. What replaces the rest is a signature that verifies, a timestamp that keeps
verifying after the certificate expires, and a release workflow that refuses to publish when either
fails.
