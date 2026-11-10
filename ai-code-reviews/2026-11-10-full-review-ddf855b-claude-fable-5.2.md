Full read of reboop at ddf855b (all of src/, tests/vm/, README.md, doc/spec.md, and the three 2026-10-11 reviews), looking for what earlier models left wrong. The code is in good shape overall: careful parsing that fails closed, deadlines everywhere, and VM tests that cover the risky paths. One design-level security problem (new with the tmux feature of 2026-11-07), one real bug (fixed), and a list of small things. Line numbers are as of 22066e9.

Fixed in this pass:

- 33a4e0c: the tmux server that a restore starts ran with `umask 077`, and so did every pane in it (`umask 077 && mkdir -p DIR && exec tmux ...` in `start_server`). Reproduced with tmux 3.8: a pane's umask was 077, and is 022 with the umask in a subshell. A resumed command made files readable by the user only, unless the pane's shell sets a umask itself (an interactive zsh on the real machines does, in /etc/zshrc, so the bug showed mostly in panes started with a command).
- 4731fe0: the reviewers of 33a4e0c pointed out that the server then came back with the transient unit's umask (0022), whatever the old server's was (027, if started from a shell on the real machines). A capture now records each server's umask, from /proc/PID/status, and the restore sets it before it execs tmux. `Server` gained an optional `umask` field for this; captures saved before it still load.
- 0d1f3fb, 22066e9, 16265b2: an overlong doc-comment line in `bounce::bounce`; a split import in check.rs.

Findings, not fixed:

- [P1] A tmux client that reboop runs as root does what any process named "tmux: server" tells it — src/tmux.rs:737
  `tmux::capture` takes every unix socket whose listening process has the name `tmux: server` (`parse_ss`, src/tmux.rs:237; any process can give itself that name) and runs `tmux -S SOCKET display -p '#{pid}'` as root, then `list-panes`. `preflight::gather` calls it, so this happens on every `reboop check`, `bounce`, `stop` and `tmux list`, with `"tmux": "block"` as much as with `"resume"`. The restore runs more root clients against the captured socket path (src/resume.rs:246, 316, 414, 673).
  A tmux client trusts its server. Before it has attached, `client_dispatch_wait` in tmux's client.c answers `MSG_SHELL` by `execl`ing the path that the server sent, `MSG_WRITE_OPEN` by opening the path that the server sent with `O_WRONLY|O_CREAT` and writing the data that the server sends, and `MSG_READ_OPEN` by sending a file's contents back (file.c). I read this in tmux 3.5a and in master; I did not build a proof of concept. (The claude-mythos-5-2 review of 33a4e0c came to the same conclusion on its own, by way of `MSG_EXEC`.)
  So any account on a machine (a person's, or a service's that has been compromised) can listen on a unix socket as `tmux: server`, speak that protocol, and have a program run as root, or a file written as root, at the next check. After a reboot there's a second way in: where /tmp is emptied at boot, anyone can create `/tmp/tmux-UID` before the restore does, and `mkdir -p` (src/resume.rs:305) takes a directory that's already there, whoever owns it. This matters on every machine that has an account which isn't already as good as root.
  Sketch of a fix: run each tmux client as the uid that owns the server process, which `capture_server` already reads from /proc/PID (e.g. `setpriv --reuid UID --regid GID --init-groups env -i tmux -S ...`), so that a hostile server gains nothing beyond its own account; and on restore, connect only if the socket's directory belongs to that user and is closed to others (tmux checks this for its default socket, but not with `-S`). I left it alone because it changes every tmux call in tmux.rs and resume.rs, and what to do about a socket directory that someone else owns is a decision.

- [P3] When asking for the reboot fails, the stopped units aren't listed — src/bounce.rs:668, 745
  `stop_and_check_again` lists what it leaves stopped whenever it doesn't go on, but if `reboot::ask` then fails with the machine still up (systemctl exits non-zero, e.g. on an inhibitor taken in that last moment, or hangs for 60 s), the stop_services stay stopped and the error doesn't name them. The earlier "X is stopped" lines are all the user has.

- [P3] One reboop's evaluation blocks another reboop's reboot — src/processes.rs:61, src/facts.rs:324
  The `nix-instantiate` that `check_rebuild` runs (for up to 3 minutes) is a "nix" blocker to any other reboop looking at the same machine. If a `reboop check` from another terminal or a timer overlaps the second check of a `bounce`, the bounce stops there with the stop_services left stopped.

- [P3] A pending capture from `reboop stop` never goes stale — src/resume.rs:99, src/bounce.rs:315
  If the machine is powered on again and nobody runs `reboop catch`, `pending` stays. `forget_outlived` only forgets captures from the current boot, so a `bounce` weeks later, with no tmux running, restores the old sessions and runs their commands again.

- [P3] A unit in stop_services that isn't a service counts as stopped uncleanly — src/reboot.rs:74
  `systemctl show --property=Result` prints nothing for a target or slice, which becomes the problem "X is stopped, but systemd's result for it is , not success" and exit status 1. Config accepts any unit name.

- [P3] Captures are named by the clock of the machine that runs reboop — src/resume.rs:93
  `Capture::new` uses the local `Timestamp::now()`, while pruning to the last 200 goes by file name on the remote machine. Two workstations with different clocks misorder them. Harmless so far.

- [P3] Smaller things in the restore, from the reviews of 33a4e0c (I confirmed each by reading the code) — src/resume.rs:295, src/check.rs:108
  A socket directly beneath / (`/tmux.sock`) gives `mkdir -p ''`, so that server never starts. The panes that a restore makes run the user's `default-command`, if their tmux.conf sets one, before a started command replaces it with `respawn-pane -k`. A pane of a window that's linked into two sessions, or of a session group, is counted once per session in the TMUX column and in "N panes".

- Limits worth a line in the README: a machine whose / isn't on a block device (a tmpfs root) fails `initrd::luks_devices` before anything is stopped, and LUKS beneath /nix alone isn't found; an idle pane whose shell has any child process (a prompt helper, say) blocks a reboot as "unknown", by design.

Still open from the 2026-10-11 reviews (I checked each against ddf855b; the rest of what they reported has been fixed):

- `systemctl reboot` still kexecs if a kernel is loaded, or soft-reboots if /run/nextroot exists (src/reboot.rs:110). `SYSTEMCTL_SKIP_AUTO_KEXEC=1 SYSTEMCTL_SKIP_AUTO_SOFT_REBOOT=1` is one line; neither happens on NixOS unless someone sets it up.
- No `ForwardAgent=no` among the forced ssh options (src/ssh.rs:93). Forcing it would break a configuration that fetches private sources over ssh during `check_rebuild`, if any does.
- `reboop check` doesn't include the initrd-address check (src/bounce.rs:190 runs only in `bounce`, for LUKS machines), so check can say "yes" where bounce then fails with an error rather than exit status 2. The README's step 1 reads as if check covered it.
- The initrd's host key isn't known to be in known_hosts until the machine is down at its initrd; a first bounce of a LUKS machine without it strands the machine until `catch`.
- Only one LUKS device beneath / is found and tested when / is a multi-device btrfs (`findmnt` names one member; src/initrd.rs:64); detached LUKS headers aren't seen; a prompt without "(press TAB for no echo)" (`password-echo=no`) isn't recognised (src/initrd.rs:14).
- GRUB: `--id` isn't matched by `find_entry` (src/boot.rs:204); a mirror whose whole grub directory is gone, or that isn't mounted, drops out of the check (src/boot.rs:292).
- The postflight compares the boot with the profile as it is afterwards, not as it was approved (src/bounce.rs:498); kernel errors are shown but aren't a problem (as the spec says); one unreachable try counts as "down" for `stop` (src/bounce.rs:707); Ctrl-C after the services are stopped leaves them stopped without a word.

Docs:

- doc/spec.md:100-101 still say "more than 1MB/s" and "load average > 1", which are settings now; line 113 says `shutdown -r now` where the code runs `systemctl reboot`; line 80 has "currenty". I didn't edit the spec, since those parts look hand-written.

Simplifications and leftovers:

- `Printer` and `indented_list` live in bounce.rs, and resume.rs imports them while bounce.rs imports resume. A small module of their own (or human.rs) would undo the cycle.
- `resume::with_machine` is the same load-find-print wrapper that `bounce::run`, `run_stop` and `run_catch` each spell out.
- `Running::Background` now means "jobs that haven't ended", some of them in the foreground; only the name (and "background" in saved captures) still says otherwise.
- Every check saves a capture even when there are no tmux servers, so /var/lib/reboop/tmux fills with 200 files that say nothing.
- Four worktrees are left over next to the checkout (reboop-fg, reboop-review-fixes, reboop-startup, reboop-tmux), with their branches and `bien` all merged into master; reboop-fg and reboop-startup have uncommitted files, so I didn't touch them.

Verification: `cargo test` passes at 22066e9: 106 unit tests and all 33 VM tests, which now start tester's tmux server with a umask of 0027 and check that it comes back with it, in a socket directory that the restore made for tester only.
