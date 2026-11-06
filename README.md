# vscu

**Keep VS Code up to date without downloading and unpacking it by hand.**

`vscu` installs and updates official VS Code Stable releases on Linux x64. It
keeps your settings, extensions, and projects separate from the application files.
It is being developed for Pop!_OS with the COSMIC desktop.

## Get started

You’ll need Rust and Cargo installed. From this repository, install the `vscu`
command:

```sh
cargo install --path . --locked
```

Then install VS Code with an icon in your application menu:

```sh
vscu install --desktop
```

For terminal access only, use `vscu install` instead. Choose `--desktop` during
this first installation; it adds the app-menu icon alongside the `code` command.
The icon follows the version selected by `vscu`, including after an update or
rollback.

If your terminal cannot find `vscu` or `code`, add their usual installation
folders to PATH for the current terminal:

```sh
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
```

Cargo’s binary folder may differ if you have customized its installation path.
To keep the PATH change, add it to your shell’s startup configuration.

Already have VS Code installed another way? `vscu` leaves that installation
alone and stops if it finds a conflicting command or user launcher. It does not
automatically replace or import an existing installation.

## Check and update VS Code

Check whether a newer version is available:

```sh
vscu check
```

Download and apply an update:

```sh
vscu update
```

If you already have the latest build, there is nothing to download. Downloads
are checked before use, and failed downloads or validation leave the active
version unchanged. Updates do not automatically switch to an older version.

### If VS Code is still open

`vscu` can download and prepare the update while you work. It calls this a
**staged update**: the new version is ready, but has not been selected yet.

Close all VS Code processes using the installation managed by `vscu`, then run:

```sh
vscu update --activate
```

This step works offline. `vscu` never closes your editor for you. If it cannot
check which processes are running, it leaves the update staged and reports the
problem. See [Current limitations](#current-limitations).

## Go back to the previous version

Close the managed VS Code processes, then run:

```sh
vscu rollback
```

This switches back to the version recorded before the last successful switch.
It works offline, provided a previous version is available.

Rollback changes the VS Code application files. Your settings and extensions
stay at their current versions.

## See what is installed

```sh
vscu
```

With no command, `vscu` shows the managed version, where it is installed, which
`code` command your terminal finds, and the available actions. You can also run
`vscu status` for the status report alone. Both work offline.

If something looks wrong, run:

```sh
vscu doctor
```

It checks local installation and integration details and explains any problems
it finds. It does not change files, launch VS Code, or disable its sandbox.

## Free up disk space

First, preview what can be removed:

```sh
vscu clean
```

Then, if you want to proceed:

```sh
vscu clean --yes
```

Cleanup keeps the active version, the previous version, any staged update, and
versions that are still running. It also preserves releases it cannot identify
as its own, changed application files, and unrelated content.

Downloaded archives are stored separately. Removing an old download does not
remove an installed version.

## Other useful commands

| What you want to do | Command |
| --- | --- |
| See all commands and options | `vscu --help` |
| See options for one command | `vscu install --help` |
| Install a particular Stable version on a fresh setup | `vscu install --version 1.100.0` (example version) |
| Build without installing the CLI | `cargo build --release` |
| Run from the source checkout | `cargo run -- status` |

## Optional configuration

The defaults work without a configuration file. If you want to change where
downloads are kept or how many versions are cached, create
`~/.config/vscu/config.toml`:

```toml
download_dir = "~/.vscu"
max_cache_count = 3
```

`max_cache_count` controls downloaded packages, not installed application
versions. It must be at least `1`. The download folder must be an absolute path
or start with `~/`. `vscu` does not create this configuration file automatically.

### Where files live

| Purpose | Default location |
| --- | --- |
| Installed application versions and version selections | `~/.local/share/vscu/` |
| Terminal command | `~/.local/bin/code` |
| Optional app-menu icon | `~/.local/share/applications/vscu-code.desktop` |
| Downloaded archives | `~/.vscu/` |
| Configuration | `~/.config/vscu/config.toml` |

If set, `XDG_DATA_HOME` replaces `~/.local/share`, and `XDG_CONFIG_HOME` replaces
`~/.config`. HOME and XDG directory overrides must be absolute paths. Changing
the data location does not change the terminal command’s location.

## Current limitations

- **Desktop verification is still pending.** Automated tests cover temporary
  installations and simulated updates. Actual terminal and COSMIC menu launches,
  plus an update and rollback with a real editor, still need acceptance testing.
- **Linux process permissions can block switching and cleanup.** If `vscu` cannot
  inspect a live process belonging to your user, it stops these actions, even if
  that process may be unrelated to VS Code. This restriction was observed on the
  development host. `vscu doctor` reports the exact error; there is no bypass.
- **Use the installation from one user account.** Process checks cover the
  installing user’s processes. They are a snapshot and cannot prevent a new
  launch at the same moment as a switch.
- **Existing installations are not migrated.** Conflicting commands and user
  launchers are preserved. If a launcher conflict appears during a download,
  VS Code may be installed successfully while launcher creation fails; the
  command reports that partial result.
- **Diagnostics do not prove that VS Code will launch.** `doctor` reports what it
  can inspect locally. Library compatibility, effective permissions, and sandbox
  readiness may still require an actual launch to verify.

## Development notes

Run the checks with:

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```

Tests use temporary installations and local HTTP responses. The optional
official-archive test needs the environment variables documented in that test.
See [GOAL.md](GOAL.md) for the feature checklist and acceptance work.

<details>
<summary>How downloads, switching, cleanup, and diagnostics work</summary>

Downloads retry temporary failures at most three times, with one- and two-second
delays and a 30-minute overall deadline. Partial downloads restart from the
beginning. Integrity and application-validation failures are not retried.
Updates also reject different builds with the same version number when their
order cannot be established.

The `current`, `previous`, and `staged` links select retained application
versions. Switching replaces `current` atomically and records the operation in
a journal. After an interruption, the next update, activation, rollback, or
confirmed cleanup recovers the bookkeeping under the installation lock. Recovery
does not select a pending target that had not already become current.

New releases include an ownership record with file checksums and link targets.
Cleanup requires that record to match before deleting a release. It preserves
unowned releases from older CLI versions and portable `data` directories. Cache
cleanup removes only recognized package files and respects `max_cache_count`.

With `/usr/bin/readelf` available, `doctor` checks direct ELF library dependencies
against common Linux x64 and application directories, plus `LD_LIBRARY_PATH`.
Transitive dependencies, custom loader paths, ABI compatibility, ACL permissions,
and sandbox readiness remain unverified until checked in the real environment.

Launcher quoting follows the [Desktop Entry specification](https://specifications.freedesktop.org/desktop-entry/latest/exec-variables.html).
Process inspection uses the kernel’s [documented proc filesystem](https://docs.kernel.org/filesystems/proc.html).

</details>
