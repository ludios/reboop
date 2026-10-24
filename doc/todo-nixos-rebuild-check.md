<!-- Model-output: Claude Fable 5.1 -->
<!-- Model-output: Claude Mythos 5.1 -->

# Todo: `reboop probe` says whether `nixos-rebuild` would change anything

Decided with the user on 2026-10-24; not implemented yet.

## Goal

A REBUILD column in `reboop probe` that says whether `nixos-rebuild switch` or `boot` on the machine would make a new system generation, i.e. whether the configuration on disk (and the nixpkgs it points to) has moved on from the system profile.  `reboop bounce` warns about it before rebooting but goes ahead.

## How: evaluate, don't build

The exact and cheap test is to evaluate the configuration's toplevel output path without building it, and compare with the system profile, which `facts::systems` already reads as `Systems::default`:

```sh
nix-instantiate --eval --raw -A config.system.build.toplevel.outPath '<nixpkgs/nixos>'
```

- Equal to `readlink -f /nix/var/nix/profiles/system`: fresh; a rebuild would do nothing new.
- Different: stale; a rebuild would make a new generation.  This is right after a rollback (the profile is behind) and after `nixos-rebuild test` (ditto), and it says fresh after `nixos-rebuild boot`, where the REBOOT column already says "new system".
- The eval fails or times out: "?", with the error below the table.

Not `nixos-rebuild dry-build`: it prints the build plan rather than the result, queries the binary caches over the network, and after a rollback or a plain `nixos-rebuild build` the target already exists in the store, so it says nothing to do while a switch would change the system.

Measured on xclank against /var/nixpkgs with a minimal configuration (a real one is a few times slower and bigger):

| Method | Time | Max RSS |
|---|---|---|
| eval of outPath, `--readonly-mode` | 1.6 to 2.0 s | 620 MB |
| eval of outPath (writes .drv files) | about 2.8 s | 620 MB |
| `nixos-rebuild dry-build --no-reexec` | 4.1 s | 620 MB |

Don't pass `--readonly-mode`: it fails for configurations that fetch during evaluation (`fetchTarball`, import-from-derivation).  Without it the eval writes .drv files (and any fetched sources), which `nixos-rebuild` would write too and which garbage collection removes.  No GC root is made.

## Assumptions

- The machines set `nix.nixPath` (the user is doing this), e.g. `[ "nixpkgs=/var/nixpkgs" "nixos-config=/etc/nixos/configuration.nix" ]`.  That lands in /etc/set-environment and /etc/pam/environment, so sshd's PAM session gives reboop's `/bin/sh` session the same NIX_PATH as root's interactive shell.  Root's ~/.zshrc is never read by the session (zsh runs `-c` non-interactively) and mustn't be needed.
- Belt and braces: if NIX_PATH is empty in the session, the script sources /etc/set-environment before evaluating (same source of truth, one line); if it's still empty, that's a failure to report, not something to guess.  Note that /etc/set-environment prepends `~/.nix-defexpr/channels` when that directory exists; harmless unless it contains a `nixpkgs` entry.
- Record the NIX_PATH the eval used in the fact, so a surprising "stale" or a "?" is diagnosable from the JSON.
- Non-flake configurations only, built the default way: `<nixpkgs/nixos>` with `nixos-config` from NIX_PATH, into the `system` profile.  If /etc/nixos/flake.nix exists, `nixos-rebuild` would use it, so the script must report "flake.nix: not supported" rather than evaluate configuration.nix.  `--file`, `--attr`, `--profile-name` and `--specialisation` aren't catered for.

## Decisions (user, 2026-10-24)

1. Its own REBUILD column, between OTHER and REBOOT: empty when fresh (or not checked), "stale", or "?".  Both store paths, or the error, on a detail line below the table.  A field in the JSON.
2. On by default, with a per-machine opt-out: `check_rebuild` in defaults.json and machines.jsonl, true unless set.  (A real eval needs roughly 1 to 3 GB of RAM; turn it off on small machines.)
3. `reboop bounce` runs the eval once, in its first preflight, and prints a warning line if the configuration is stale (or the eval failed), then proceeds.  The second check (`check_again`) skips the eval.  Not a blocker, not among the problems bounce lists at the end.

## Steps

1. **facts.rs: the fact and how it's found.**
   - A type like:
     ```rust
     /// What the configuration on disk evaluates to, which is what
     /// `nixos-rebuild switch` or `boot` would make the system profile.
     pub enum Rebuild {
         /// The toplevel it evaluates to (a store path), to compare with
         /// `Systems::default`.
         Evaluated { toplevel: String, nix_path: String },
         /// Why it couldn't be evaluated: nix-instantiate's stderr, that it
         /// took longer than the timeout, or that the machine uses a flake.
         Failed { error: String, nix_path: String },
     }
     ```
     plus a method that answers "stale?" given the `Systems` (or a free function in preflight.rs).  Failure is a value, never an `Err` out of `gather`, or one broken configuration makes the whole machine "couldn't be checked".
   - `pub fn rebuild(session: &mut Session) -> Result<Rebuild>` runs one script: check for /etc/nixos/flake.nix; source /etc/set-environment if NIX_PATH is empty; print NIX_PATH; then
     ```sh
     nice -n 19 timeout --kill-after=5 180 nix-instantiate --eval --raw -A config.system.build.toplevel.outPath '<nixpkgs/nixos>'
     ```
     Pass the output through `store_path` (it must be a bare /nix/store entry, or it's a failure).  Treat timeout's 124 and 137 like smart.rs does.  The session timeout must be longer than the remote one (180 s plus kill-after plus slack), since a command that outlives its session timeout breaks the session.  QUICK (30 s) is too short for an eval.
   - A const for the 180 s with a docstring saying why that long (cold page cache on a slow disk, big configurations).

2. **preflight.rs: `Facts.rebuild: Option<Rebuild>`** (`None` when not checked), and `gather(session, hostname, check_rebuild: bool)`.
   - Run it early, with the other slow things (after SMART, before the quick facts), so the quick facts stay fresh.  It can't count itself as Nix activity: session commands run one after another, so nix-instantiate has exited before the process listing.  A probe or bounce running at the same time from elsewhere may count it as a Nix command for a few seconds; accepted.
   - No new blocker.

3. **config.rs: `check_rebuild`.**  Field in `Defaults`, `MachineLine` and `Machine`, resolved in `resolve` with a `DEFAULT_CHECK_REBUILD: bool = true` const, documented like `root_full_percent`.

4. **probe.rs: the column, the details, the JSON.**
   - `HEADER` gets "REBUILD" between "OTHER" and "REBOOT" (16 columns); `table_row` gets a cell: `Plain.cell("")` for `None` or fresh, `Plain.cell("stale")`, or `Plain.cell("?")`.  Not red: it doesn't block a reboot.
   - `table`'s detail lines: for stale, `HOST: nixos-rebuild would change the system: PROFILE → EVALUATED` (store paths; the kernel cell already uses →); for failed, `HOST: couldn't evaluate the configuration: ERROR` with further lines indented four spaces like an error's.
   - `JsonReport::Checked` gets `needs_rebuild: Option<bool>` next to `needs_reboot`; `facts.rebuild` serializes with the rest.
   - Update the `probe` subcommand's doc comment in main.rs if it enumerates what the table shows.

5. **bounce.rs: warn and proceed.**  `probe` (bounce's) passes `machine.check_rebuild` to `gather`; `check_again` passes false.  If stale or failed, print one line (there's no yellow in `human::Style`; Bold is enough): `A nixos-rebuild would change HOST: PROFILE → EVALUATED` or `Couldn't tell whether a nixos-rebuild would change HOST: ERROR`.  Then carry on exactly as now.

6. **Docs.**  README's probe sentence and the configuration mention; doc/spec.md's configuration example (`"check_rebuild": true`), the preflight list of facts, and what probe shows; this file is deleted once done (or kept as a design note if the user prefers, say which in the commit).

7. **Tests.**
   - Unit: `store_path` rejecting an eval that printed something else; `reboot_reasons`-style test of the stale decision; probe's table and JSON with `idle_facts()` set to fresh, stale, failed and `None`; config parsing of `check_rebuild` with and without the default.
   - VM: give the test systems a stub nixpkgs so the eval is cheap and steerable: `nix.nixPath = [ "nixpkgs=/etc/reboop-test-nixpkgs" "nixos-config=/etc/nixos/configuration.nix" ]` in tests/vm/common.nix, with `environment.etc."reboop-test-nixpkgs/nixos/default.nix".text` being an attrset whose `config.system.build.toplevel.outPath` is `builtins.readFile /var/lib/reboop-test/toplevel` (write the file without a trailing newline).  Then the test writes the base or alt system's path there and checks: fresh (empty cell, `needs_rebuild: false`), stale (cell, detail line with both paths, `needs_rebuild: true`), the file missing ("?" and an error line), `check_rebuild: false` (`None`, empty cell), and that bounce prints its warning after `nix-env --set` to the other system and still reboots.  This also exercises getting NIX_PATH into the eval (through PAM, or the set-environment fallback) in a stock VM whose root shell is bash; log which, to learn whether the fallback is ever needed.
   - `cargo test` in full, since the VM tests change.

8. **Housekeeping.**  Model-output lines in files touched; commit per AGENTS.md; the five reviews.

## Costs and limits

- A few seconds per machine per probe on a warm cache; machines are probed in parallel, so the wall time is the slowest machine's.  RAM is the real cost (hundreds of MB to a few GB), hence the opt-out.
- The check says *that* a rebuild would change the system, not *what*.  Saying what needs a build (`nix store diff-closures` between the profile and the result); out of scope.
- A `nixos-rebuild` running at the same time is harmless to the eval, and the NIX column shows it anyway.

## Later, maybe

- Flakes: evaluate `/etc/nixos#nixosConfigurations.HOST.config.system.build.toplevel.outPath` with `nix eval --raw` when flake.nix exists.
- Have the eval not count as Nix activity for concurrent reboop runs, e.g. by skipping `nix-instantiate --eval` in processes.rs (interrupting an eval wastes nothing).
