<!-- Model-output: Claude Fable 5.1 -->
<!-- Model-output: Claude Mythos 5.1 -->

# Todo: `reboop probe` says whether `nixos-rebuild` would change anything

Decided with the user on 2026-10-24 and reviewed by five models the same day; not implemented yet.

## Goal

A REBUILD column in `reboop probe` that says whether `nixos-rebuild switch` or `boot` would make the system profile point at another system, i.e. whether the configuration on disk (and the nixpkgs it points to) has moved on from the profile.  `reboop bounce` warns about it before rebooting but goes ahead.  (Not "a new generation": `nix-env --set` reuses the last generation when the path matches it, as after a rollback.)

## How: evaluate, don't build

Evaluate the configuration's toplevel output path without building it, and compare with the system profile, which `facts::systems` already reads as `Systems::default`:

```sh
nix-instantiate --eval --raw -A config.system.build.toplevel.outPath '<nixpkgs/nixos>'
```

- Equal to the profile: fresh; a rebuild would change nothing.
- Different: stale.  That's right after a rollback or `nixos-rebuild test` (the profile is behind), and it says fresh after `nixos-rebuild boot`, where the REBOOT column already says "new system".
- The eval fails or times out: "?", with the error below the table.

Not `nixos-rebuild dry-build`: it prints the build plan rather than the result, queries the binary caches over the network, and after a rollback or a plain `nixos-rebuild build` the target already exists in the store, so it says nothing to do while a switch would change the system.

`nix-instantiate --eval` is read-only unless given `--read-write-mode`.  Stay read-only, so a probe never builds or writes to the store: a configuration that needs import-from-derivation fails with "path '….drv' is not valid" and shows "?".  Accepted; `--read-write-mode` would make a routine probe build (and fetch) whatever such a configuration imports.

Measured on xclank against /var/nixpkgs with a minimal configuration (a real one is a few times slower and bigger):

| Command | Time | Max RSS |
|---|---|---|
| the eval above (read-only) | 1.5 to 2.0 s | 620 MB |
| the eval with `--read-write-mode` | 2.4 s | 620 MB |
| `nix-instantiate -A system` (writes every .drv) | 2.8 s | 620 MB |
| `nixos-rebuild dry-build --no-reexec` | 4.1 s | 620 MB |

## Assumptions

- The machines set `nix.nixPath`, e.g. `[ "nixpkgs=/var/nixpkgs" "nixos-config=/etc/nixos/configuration.nix" ]` (the user is doing this).  NixOS puts that in /etc/set-environment, which /etc/zshenv sources, and in /etc/pam/environment, which sshd's PAM session applies, so reboop's `/bin/sh` session has the same NIX_PATH as root's interactive shell.  Root's ~/.zshrc is never read by the session (zsh runs `-c` non-interactively) and mustn't be needed.
- If NIX_PATH is empty in the session, that's a failure to report ("?"), not something to guess or to patch up by sourcing /etc/set-environment, which has nothing the session doesn't already have.
- Record the NIX_PATH the eval used in the fact, so a surprising "stale" or a "?" can be diagnosed from the JSON.
- Only the default way of building: `<nixpkgs/nixos>` with `nixos-config` from NIX_PATH, into the `system` profile.  nixos-rebuild-ng (`BuildAttr.from_arg` in models.py) prefers, in order: /etc/nixos/flake.nix, a `<nixos-system>` NIX_PATH entry, /etc/nixos/system.nix, and only then `<nixpkgs/nixos>`.  The script checks for the first three (`nix-instantiate --find-file nixos-system` for the second) and reports "not supported" rather than evaluate the wrong thing.  `--file`, `--attr`, `--profile-name` and `--specialisation` aren't catered for.

## Decisions (user, 2026-10-24)

1. Its own REBUILD column, between OTHER and REBOOT: "ok" when fresh (green, like SMART's), "stale", "?" when the eval failed, and empty when not checked, so a machine that opted out doesn't look fresh.  Both store paths, or the error, on a detail line below the table.  A field in the JSON.
2. On by default, with a per-machine opt-out: `check_rebuild` in defaults.json and machines.jsonl, true unless set.  (A real eval needs roughly 1 to 3 GB of RAM; turn it off on small machines.)
3. `reboop bounce` runs the eval once, after its preflight finds no blockers, and prints a warning line if the configuration is stale (or the eval failed), then proceeds.  Not a blocker, not among the problems bounce lists at the end, and not run by `reboop stop` or by the second check before rebooting.

## Steps

1. **facts.rs: the fact and how it's found.**  Separate from `gather`, called after it, so the eval can't skew the other facts (the load average it adds to, the job and network sample it would delay, the process listing it could appear in if killed) and no flag has to go through `gather`'s dozen callers.
   - A type like:
     ```rust
     /// Whether `nixos-rebuild switch` or `boot` would change the system
     /// profile, found by evaluating the configuration without building it.
     #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
     #[serde(tag = "state", rename_all = "snake_case")]
     pub enum Rebuild {
         /// The configuration evaluates to the profile's toplevel.
         Fresh { toplevel: String, nix_path: String },
         /// It evaluates to another toplevel, which a rebuild would switch to.
         Stale { toplevel: String, nix_path: String },
         /// Why it couldn't be evaluated: nix-instantiate's stderr, that it
         /// was killed or took too long, that NIX_PATH is empty, or that the
         /// machine is configured in a way that isn't supported.
         Failed { error: String, nix_path: String },
     }
     ```
     Failure is a value, never an `Err` out of the function, or one broken configuration makes the whole machine "couldn't be checked"; an eval that prints something other than a bare store path (`store_path` says) is `Failed` too.
   - `pub fn rebuild(session: &mut Session, profile: &str) -> Result<Rebuild>` runs one script that prints NIX_PATH, checks the entry points above and that NIX_PATH isn't empty, then runs
     ```sh
     choom -n 1000 nice -n 19 timeout --kill-after=5 180 nix-instantiate --eval --raw -A config.system.build.toplevel.outPath '<nixpkgs/nixos>'
     ```
     and prints its exit status and how long it took.  `choom -n 1000` (util-linux) makes the eval the OOM killer's first pick rather than some service; `nice` spares the machine's CPU but not its load average, which is one reason this runs after `gather`.  Exit 124 means it took longer than the timeout; 137 means SIGKILL, from timeout's kill-after or from the OOM killer, so say "killed after N s (out of memory, or stuck past the timeout)" rather than blame the timeout.  The session timeout must exceed the remote one (180 s plus kill-after plus slack), since a command that outlives its session timeout breaks the session; QUICK (30 s) is far too short.
   - A const for the 180 s with a docstring saying why that long (cold page cache on a slow disk, big configurations).

2. **config.rs: `check_rebuild`.**  Field in `Defaults`, `MachineLine` and `Machine`, resolved in `resolve` with a `DEFAULT_CHECK_REBUILD: bool = true` const, documented like `root_full_percent`; `preflight::test_machine()` sets it.

3. **probe.rs: the column, the details, the JSON.**
   - `check_machine` calls `facts::rebuild` after `gather` when `machine.check_rebuild`, and `Outcome` carries the `Option<Rebuild>` beside the facts and blockers.
   - `HEADER` gets "REBUILD" between "OTHER" and "REBOOT" (16 columns); the cell is `Green.cell("ok")`, `Plain.cell("stale")`, `Plain.cell("?")` or `Plain.cell("")`.  Not red: it doesn't block a reboot.
   - `table`'s detail lines: for stale, `HOST: nixos-rebuild would change the system: PROFILE → EVALUATED` (the kernel cell already uses →); for failed, `HOST: couldn't evaluate the configuration: ERROR`, with further lines indented four spaces like an error's.
   - `JsonReport::Checked` gets `rebuild: Option<&Rebuild>` next to `needs_reboot`: `null` when not checked, otherwise `{"state": "fresh" | "stale" | "failed", ...}` with the fields above, so a consumer can tell off from broken.
   - Update the `probe` subcommand's doc comment in main.rs.

4. **bounce.rs: warn and proceed.**  In `bounce` only (its `probe` helper is shared with `stop`), once `probe` says okay and before services are stopped: if `machine.check_rebuild`, call `facts::rebuild`, and for stale or failed print one line (there's no yellow in `human::Style`; Bold is enough): `A nixos-rebuild would change HOST: PROFILE → EVALUATED` or `Couldn't tell whether a nixos-rebuild would change HOST: ERROR`.  Then carry on exactly as now.  (`check_again` ignores load and network anyway.  A nix-instantiate that timeout killed but that's still tearing down could show up in its process listing as Nix activity, which is the one way the eval can block a bounce; accepted.)

5. **Docs.**  README's probe sentence and the configuration mention; doc/spec.md's configuration example (`"check_rebuild": true`), the preflight list of facts, and what probe shows.  Delete this file once done.

6. **Tests.**
   - Unit: the parser of the script's output (the NIX_PATH line, a result with no trailing newline, exit 124, 137 and 1, the not-supported cases, an empty NIX_PATH); the fresh/stale comparison; probe's table and JSON for fresh, stale, failed and not checked; config parsing of `check_rebuild` with and without the default.  (`store_path` is tested already.)
   - VM: give the test systems a stub nixpkgs so the eval is cheap and steerable: `nix.nixPath = [ "nixpkgs=/etc/reboop-test-nixpkgs" "nixos-config=/etc/nixos/configuration.nix" ]` in tests/vm/common.nix, with `environment.etc."reboop-test-nixpkgs/nixos/default.nix".text` being an attrset whose `config.system.build.toplevel.outPath` is `builtins.readFile /var/lib/reboop-test/toplevel` (written without a trailing newline).  The test writes the profile's path there and checks fresh ("ok", `"state": "fresh"`); writes the other system's path and checks stale (the cell, the detail line with both paths, `"state": "stale"`) without touching the profile, which would make the boot loader a blocker; removes the file and checks "?" with an error line; sets `check_rebuild: false` and checks the empty cell and `null`; and checks that bounce prints its warning when stale and still reboots.  The VM's root shell is zsh (common.nix), so NIX_PATH reaches the session through /etc/zshenv's set-environment and through PAM alike; the test proves that it arrives.
   - `cargo test` in full, since the VM tests change.

7. **Housekeeping.**  Model-output lines in files touched; commit per AGENTS.md; the five reviews.

## Costs and limits

- A few seconds per machine per probe on a warm cache, after the other facts; machines are probed in parallel, so the wall time is the slowest machine's.  RAM is the real cost (hundreds of MB to a few GB), hence `choom` and the opt-out.
- The check says *that* a rebuild would change the system, not *what*.  Saying what needs a build (`nix store diff-closures` between the profile and the result); out of scope.
- A `nixos-rebuild` running at the same time is harmless to the eval, and the NIX column shows it anyway.  A probe or bounce running at the same time from elsewhere may count the eval as a Nix command for a few seconds; accepted.

## Later, maybe

- Flakes: evaluate `/etc/nixos#nixosConfigurations.HOST.config.system.build.toplevel.outPath` with `nix eval --raw` when flake.nix exists.  system.nix and `<nixos-system>` likewise, once it's clear what they evaluate to.
- Have the eval not count as Nix activity for concurrent reboop runs, e.g. by skipping `nix-instantiate --eval` in processes.rs (interrupting an eval wastes nothing).
- A memory cap (`systemd-run --scope -p MemoryMax=…`) if `choom` and the opt-out turn out not to be enough.
