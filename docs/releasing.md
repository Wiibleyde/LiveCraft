# Releasing

A tag named `v` + the version in `Cargo.toml` publishes a release for Windows:

```sh
# Cargo.toml says version = "0.2.0", the commit is on main and CI is green
git tag v0.2.0
git push origin v0.2.0
```

`.github/workflows/release.yml` then builds, signs when it can, installs and uninstalls the result
on the runner, and creates the GitHub Release with these files:

| File | What it is |
| --- | --- |
| `LiveCraft-<version>-windows-x64-setup.exe` | The installer |
| `LiveCraft-<version>-windows-x64-portable.zip` | The same files in a folder, to unzip and run |
| `SHA256SUMS.txt` | Checksums of both, for `sha256sum -c` or `Get-FileHash` |

Unsigned builds have `-unsigned` in every file name and a warning at the top of the release notes. A
tag with a `-`, such as `v0.2.0-rc.1`, makes a pre-release. The workflow refuses a tag that differs
from `Cargo.toml`.

To try the whole thing without publishing, run the **Release** workflow by hand from the Actions tab:
it does everything but create the release, and keeps the files as a workflow artifact.

## The installer

Built by [Inno Setup](https://jrsoftware.org/isinfo.php) from `packaging/windows/livecraft.iss`. The
workflow installs a pinned version and checks its SHA-256 first.

- Installs for the current user, in `%LOCALAPPDATA%\Programs\LiveCraft`, without administrator
  rights. `/ALLUSERS` installs for everyone instead.
- Adds a Start menu entry, and a desktop shortcut if asked.
- Makes `.livecraft` files open in LiveCraft (`livecraft.exe --project <file>`), and offers LiveCraft
  in "Open with" for `.vmix` and `.regie` files without taking them over.
- Uninstalls from Apps & features, removing the program, its entries and its file types. The session
  in `%APPDATA%\LiveCraft` and the user's projects and recordings stay.
- Unattended: `setup.exe /VERYSILENT /SUPPRESSMSGBOXES /DIR=<folder>`, and
  `unins000.exe /VERYSILENT` in that folder to remove it.

## FFmpeg is not included

LiveCraft needs FFmpeg, but releases do not ship it. The usual Windows builds (gyan.dev, BtbN) are
GPL, and handing out a GPL program with ours brings obligations (offering its exact source, among
others) that deserve a deliberate decision, not a side effect of packaging. Until then:

- the installer looks for `ffmpeg.exe` where LiveCraft will (the program's folder, then `PATH`), and
  when it is missing shows a page explaining how to get it, with a link to the download page;
- `FFmpeg.txt` sits next to `livecraft.exe` in both the installer and the zip;
- the release notes say so.

Copying `ffmpeg.exe` into the LiveCraft folder works because Windows looks there first for a program
started by name.

## Two builds in one package

`.cargo/config.toml` builds for `x86-64-v3` (AVX2 and friends), because the pixel loops gain a lot
from wider instructions. Such a program crashes on a processor without them, and Windows 10 still
runs on many. A release therefore holds two builds of the same code:

- `livecraft.exe`, built for plain `x86-64`, which every 64-bit Windows processor runs;
- `livecraft-avx2.exe`, built for `x86-64-v3`.

Shortcuts and file types start `livecraft.exe`. When the processor has every feature `x86-64-v3`
assumes and `livecraft-avx2.exe` is beside it, it starts that one with the same arguments, waits, and
exits with its exit code (`hand_over_to_avx2_build` in `src/main.rs`). Otherwise it runs itself, and
so it does when `livecraft-avx2.exe` cannot start or dies within two seconds with a Windows crash
status (illegal instruction, access violation, missing library...). The wrapper and the program it
started end together: killing one kills the other.
Development builds have the AVX2 features enabled, so the hand-over is compiled out of them.

What the second build is worth, measured with a throwaway benchmark of the compositing primitives at
1080p, on one thread, best of three runs of 20, on an AMD Ryzen 7 5800X shared with other work:

| Per picture | `x86-64` | `x86-64-v2` | `x86-64-v3` |
| --- | ---: | ---: | ---: |
| Blend over | 16.97 ms | 12.51 ms | 6.61 ms |
| Scale to half size | 7.50 ms | 6.95 ms | 4.63 ms |
| Mix of two | 5.01 ms | 2.44 ms | 1.26 ms |
| Colour correction | 18.69 ms | 18.62 ms | 10.41 ms |
| Chroma key | 26.13 ms | 33.90 ms | 28.18 ms |
| **Total** | **74.31 ms** | **74.42 ms** | **51.08 ms** |

The baseline build takes about 45 % longer for the same work; `x86-64-v2` gains nothing over it on
the whole, so there is no third build. The chroma key barely changes, as it spends its time in
scalar floating point.

Both were checked under the [Intel SDE](https://www.intel.com/content/www/us/en/download/684897/intel-software-development-emulator.html)
posing as a Core 2, which has none of these features (`sde -mrm -chip_check_exe_only -- livecraft.exe
--screenshot shot.png`): `livecraft.exe` stayed on itself, opened the window and composed the worked
example, while `livecraft-avx2.exe` stopped on its first AVX instruction (`vmovups ymm0`).

## Signing

The workflow signs `livecraft.exe`, `livecraft-avx2.exe`, the installer and the uninstaller inside it
with Authenticode (SHA-256, timestamped) when these repository secrets exist:

| Secret | Holds |
| --- | --- |
| `WINDOWS_CERTIFICATE` | The code signing certificate as a base64 `.pfx`, with its private key and chain |
| `WINDOWS_CERTIFICATE_PASSWORD` | The `.pfx` password |

```powershell
[Convert]::ToBase64String([IO.File]::ReadAllBytes('codesign.pfx')) | gh secret set WINDOWS_CERTIFICATE
gh secret set WINDOWS_CERTIFICATE_PASSWORD
```

`packaging/windows/certificate.ps1` puts the certificate in the runner's user store straight from
memory, so neither the `.pfx` nor its password reaches the disk or a command line; `signtool` then
finds it by thumbprint, and the last step (which runs even when others fail) takes it out again. Both
programs are built before the certificate is imported (`package.ps1 -BuildOnly`, then `-NoBuild`), so
no dependency's build script ever runs with the key available. The workflow checks every signature
and timestamp before publishing.

Certificates issued since June 2023 by public authorities keep their key in hardware or a cloud
service and cannot be exported as a `.pfx`. Using one of those means replacing the import step with
that service's `signtool` plug-in (`/dlib`); the rest of the workflow stays the same.

Without the secrets, the build is published unsigned and says so. Windows SmartScreen then warns
before running it.

## Building it yourself

You need Rust, [Inno Setup](https://jrsoftware.org/isdl.php) 6.3 or later (or `ISCC` set to its
`ISCC.exe`), and PowerShell 7 or Windows PowerShell 5.1. For signing, the Windows SDK's `signtool`.

```powershell
./packaging/windows/package.ps1                          # unsigned, into target/dist
./packaging/windows/package.ps1 -CertThumbprint <sha1>   # signed with a certificate in your store
./packaging/windows/install-check.ps1 (Get-Item target/dist/*-setup.exe).FullName -Screenshot target/shot.png
```

A tag release also needs a tag on `main`, and runs `cargo fmt --check`, `clippy` and the tests first;
the actions and the Rust version it uses are pinned.

`install-check.ps1` refuses to run when LiveCraft is installed for the current user (it would
uninstall it) unless `-Force` is given. It installs silently into a temporary folder, checks the Start menu entry, the file
types and the uninstall entry, starts the program, then uninstalls and checks nothing is left. The
workflow runs it without `-Screenshot`, as hosted runners have no OpenGL.
