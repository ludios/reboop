This is a special-purpose utility for carefully rebooting a NixOS machine, which works in stages.

## General notes

### AI says:

- Set these on every connection attempt: `BatchMode=yes`, `StrictHostKeyChecking=yes`, `ConnectTimeout=15`, `ServerAliveInterval=5`, `ServerAliveCountMax=3` and `ControlPath=none`
    - `StrictHostKeyChecking=yes` matters if your ssh config says `accept-new`: a first login would otherwise add an impostor's key, which the login that sends the LUKS password would then trust.
    - `ControlPath=none` matters if your ssh config uses `ControlMaster`/`ControlPersist`: a post-reboot attempt can otherwise reuse the dead shared connection and hang.
    - The keepalive matters because the initrd can drop the network without cleanly closing the connection.
    - Give every wait loop an overall deadline

- Use the ssh binary rather than a Rust SSH library, so our ssh config, agent and known_hosts all just work.

### Managing LUKS keys:

```
printf reboop-luks-v1 | ssh-keygen -Y sign -n reboop-luks -f ~/.ssh/id_ed25519.pub
```

and hash the signature into a 256-bit master key;

use that master key with XChaCha20-Poly1305 to encrypt / decrypt files storing LUKS passwords in:

```
~/.config/reboop/luks/HOSTNAME
```

## Configuration file

`~/.config/reboop/defaults.json` (JSON5)

```
{
    "ssh_port": 22,
    "initrd_ssh_port": 23,
    "scrub_mounts": ["/"],
    "scrub_every_days": 14,
    "max_network_transfer_bytes_per_sec": 1000000,
    "max_load_average_1min": 2,
    "root_full_percent": 97,
    "check_rebuild": true,
    "luks_signing_key": "~/.ssh/id_ed25519.pub",
    "stop_services": ["postgresql"],
    "tmux": "resume",
    "resumable_commands": ["yt-dlp .*", "(?s)crawl .*"],
}
```

`~/.config/reboop/machines.jsonl`

```
{"hostname": "one", "ipv4": "...", "ssh_port": 2222, "stop_services": []}
{"hostname": "two", "ipv4": "...", "scrub_mounts": ["/", "/small"]}
{"hostname": "three", "ipv4": "...", "max_network_transfer_bytes_per_sec": 1000000, "max_load_average_1min": 2}
{"hostname": "four", "ipv4": "...", "tmux": "block", "resumable_commands": ["htop"]}
```

A machine's settings replace the defaults, except `resumable_commands`, which are added to the defaults'. Each is a regular expression (in the syntax of Rust's regex crate: no look-around or backreferences) that has to match a whole command line (so `yt-dlp .*`, not `yt-dlp`; `(?s)` lets `.` match line breaks). A pane that tmux started with a command rather than a shell is matched as tmux shows the command, quoted for tmux's own parser: `"sleep 600"`, with the quotes, for `tmux new-window 'sleep 600'`. `tmux` is `block` (any tmux server keeps the machine from going down) unless set to `resume` (see Tmux below).

## Preflight

1. SSH in to the machine and get information about these things:

    - Is any btrfs scrub running?
    - Is any btrfs balance or drive replace in progress?
    - Is switch-to-configuration or a nix build running?
    - Every tmux server (any user's, at any socket: `ss -xlpH` lists the sockets with their processes; a `tmux: server` process that ss shows no socket for, as in a container's network namespace, is noted as unlistable), and what's in it, with `tmux -S SOCKET list-panes -a` as root, after checking that the server answering at the socket is the one ss showed: each session (and its session group, if any), window (index; name, unless automatic-rename is on; layout, size included; whether active) and pane (index; tmux's id; whether active), and what the pane is running (nothing, if it's dead, as remain-on-exit keeps panes): the jobs its shell started that haven't ended, whether in the background or stopped (async_start records not matched by as many async_end or async_lost records in the zsh-spy archive, hist.*.jsonl, that the shell holds open, found through /proc/PID/fd), whatever's in the foreground, except for a job that was stopped with Ctrl-Z and brought back with fg: that counts as the command line that started the job, typed where that line was typed, if all of these hold: the job is the shell's only job that hasn't ended; the terminal's foreground process group is the job's (led by the first of the pids in its async_start's job_pids); the line the shell runs (its last command_start, without a command_end) is nothing but `fg`, `fg JOB` or `%JOB`, where JOB is made of letters, digits and `_%+?.-` or is left out, both as typed and with its aliases expanded (a shell function named fg isn't seen through); zsh-spy found the job stopped (its async_start's job_state is "suspended"); and the line that started the job ended stopped, having started that job alone (that line's command_end has status 128 + SIGTSTP or 128 + SIGSTOP, and its async_jobs lists only the job). So a job put in the background with & still counts as a job that hasn't ended once fg brings it into the foreground, as does a loop or function stopped with Ctrl-Z until the command that it was running when stopped ends (zsh gives that command a process group of its own); else the command line typed into the shell, the archive's last command_start without a command_end; else idle. Without an archive: the command tmux started the pane with (`#{pane_start_command}`), or idle if the pane's program is a shell without child processes; and unknown when a program is in the foreground but there's no archive, or the archive's line is hidden (HIST_IGNORE_SPACE), or is nothing but fg (as when the job it brought back isn't known), or the shell has a child process the archive doesn't account for: any child while the archive says the shell is idle, or, while a typed command runs, a child outside the terminal's foreground process group (like a disowned job). The pane's directory is where a typed command was typed (as typed, so perhaps through a symlink), where a started command was started, or else the pane's current one; also where the filesystem it's on is mounted (`findmnt -T`), for checking it's back. Of an archive, which is read through /proc/PID/fd, only these records (lines) are used: those from the last command_start on; and from before that, however far back, the async_start, async_end and async_lost records of each job that hasn't ended (by command id and job number, one with more async_starts than async_ends and async_losts, since zsh can give a job the number of one that ended during the same line), the command_start and command_end of the line that started such a job, and the records of a job whose async_end or async_lost was written before its async_start, if that async_start comes after the last command_start. A server whose panes can't be listed (e.g. its tmux is another version, or a record in an archive can't be parsed) is noted as such, which keeps the machine from going down either way.
    - Is a copy program running: rsync, cp, mv, scp, dd, ddrescue, pv, or `rclone` with one of the subcommands copy, copyto, copyurl, sync, move, moveto, bisync, cat, rcat or archive?
    - Is a btrfs send or receive running, or cryptsetup?
    - Is a disk tool running: `fsck` or `fsck.*`, `e2fsck`, `btrfsck`, `xfs_repair`, `btrfs check`, `btrfs rescue` or `btrfs restore`, `btrfs-convert`, `btrfstune`, `mkfs` or `mkfs.*`, `mke2fs`, `resize2fs`, `badblocks`, or a partitioner (`fdisk`, `cfdisk`, `sfdisk`, `gdisk`, `sgdisk` or `parted`)?
    - Is any btrfs device missing, or have its error counters (sysfs devinfo/*/error_stats) gone above zero?
    - If the machine has smartctl: what SMART says about each whole disk beneath the btrfs filesystems' devices (`lsblk --inverse` from sysfs' devices/*, then `smartctl -n standby -H -A` on all the disks at once, each with a timeout, leaving any in standby asleep, as spinning up can take a minute, and likewise any whose power mode smartctl can't read, which it takes to be asleep): its overall health, and the counts of sectors gone bad (ATA attributes 5, 187, 197 and 198 when their names say so, as vendors put other counters there too; for NVMe, the critical warning except its temperature bit, and the media errors). Whatever smartctl could read counts, and what it couldn't is noted.
    - Inhibitor locks (logind's ListInhibitors)
    - systemd jobs that last through the network sample
    - How full the root filesystem is (df)
    - Network bytes in / out over 5 seconds
    - Load average over the last minute
    - The NixOS configuration we're currently on
    - The Linux kernel we're currenty on
    - The NixOS configuration and kernel we expect to boot into by default
    - What the boot loader will boot by default: systemd-boot's default entry (`bootctl list`, which includes one-shot entries), or GRUB's in each /boot with a grub/grub.cfg (mirroredBoots), including grub-reboot's next_entry
    - The kernel, kernel modules and initrd the machine booted with, the systemd PID 1 runs (switching re-executes it), and the system profile's, so `reboop check` can say why a reboot is needed: "new system" if the profile isn't the current configuration, and "new X" or "rebuilt X" for each part the profile has another version or build of (a kernel's modules and initrd come with it)
    - After the rest, if check_rebuild is on: whether `nixos-rebuild switch` or `boot` would change the system profile, i.e. whether the configuration evaluates to another toplevel than the profile's: `nix-instantiate --eval --raw --option allow-import-from-derivation false -A config.system.build.toplevel.outPath '<nixpkgs/nixos>'`, niced, as the OOM killer's first pick (`choom -n 1000`), within 3 minutes. Nothing is built (a configuration that imports from a derivation fails), though sources the configuration fetches (e.g. with `builtins.fetchTarball`) get downloaded into the store. Only that default way of building is supported: if nixos-rebuild would build /etc/nixos/flake.nix, `<nixos-system>` or /etc/nixos/system.nix instead, or looking for `<nixos-system>` fails (within 30 seconds), or NIX_PATH is empty in the session, it's a failure, as is an evaluation that fails. `reboop check` shows "ok", "stale", "?" for a failure, or nothing if it's off, and below the table, says what it would change or why it failed (with `-v`, or when machines are named, as in `reboop check HOSTNAME`); `reboop bounce` warns about either but goes ahead. It's no reason not to reboot.

2. Decide whether the machine is okay to reboot.

    - If the boot loader's default doesn't boot the system profile, or its kernel or initrd is missing, or it has the initrd take a static address (ip=) other than the machine's ipv4, no.
    - If any btrfs scrub is running, we can't; Linux will hang on shutdown due to the scrub.
    - If any btrfs balance or drive replace is running, no.
    - If switch-to-configuration or a nix build running, no.
    - If any tmux server is running and the machine's `tmux` is `block`, no. If it's `resume`: no if a pane runs a command that none of the machine's `resumable_commands` matches as a whole, or a command whose directory is gone, or what a pane runs is unknown, or a pane's shell has a job that hasn't ended (in the background, stopped, or brought back into the foreground, unless it counts as the line that started it: see above), or a server couldn't be listed; idle panes and resumable commands are fine, to be brought back after the reboot (see Tmux below). A command that's a reason not to reboot by the rules above (a copy, say) still is, whatever it matches.
    - If a copy program is running, no.
    - If a btrfs send or receive, cryptsetup or a disk tool is running, no.
    - If a btrfs device is missing (the filesystem wouldn't mount at boot without the degraded option) or has had errors, no.
    - If a disk's SMART self-assessment failed, or any of those counts is above zero, or smartctl got no answer from the disk within its timeout, no. (A disk whose SMART smartctl can't read, like a virtual one or one in standby, doesn't count, but `reboop check` marks it.)
    - If an inhibitor lock asks for no shutdown (mode block or block-weak), no.
    - If a systemd job has lasted 5 seconds, no.
    - If the root filesystem is root_full_percent (97%) used or more, no.
    - If more than 1MB/s being transferred over the network, no.
    - If load average over the last minute > 1, no.

## Reboot

1. SSH in and:

    `systemctl stop` each of stop_services, in order, skipping any the machine doesn't have. Stop each one in the same `systemctl stop` (one transaction, with systemd 262 or later) as the units that trigger it (its `TriggeredBy=`, such as socket and timer units) and that aren't inactive or failed, so that they don't start it again. If stopping one fails, say which units it stopped are still stopped, and that the one that failed, and the units that trigger it, may be stopped as well, and leave it at that.

    Check again as in Preflight, except for network traffic and load average, which stopping services changes. If it's no longer okay to reboot, say so, and which units it stopped are still stopped, and leave it at that.

    Save what that check found in the tmux servers on the machine, as `/var/lib/reboop/tmux/TIME.reboot.json` (`.shutdown.json` for a stop; TIME to the millisecond), written whole and synced, and if there's anything in them, point `/var/lib/reboop/tmux/pending` at it, and say what will be run again; if there isn't, remove `pending` if it points to a capture taken for a reboot or shutdown during this boot, which didn't happen, and whose servers are gone now. If that fails, say which units it stopped are still stopped, and leave it at that. (Every `reboop check` and `reboop tmux list` saves a `TIME.check.json` there too, without `pending`, though a check goes on if that fails; the last 200 captures are kept, for root only, plus the pending one.)

    `shutdown -r now`

2. After disconnection, keep trying to SSH in over initrd_ssh_port (and ssh_port, in case it was unlocked at the console), with a 15 second timeout, once every 15 seconds:

    `ssh root@[ipv4 address of machine] -p [initrd_ssh_port]`

    until success.

    Or (AI says):
    
    ```
    ssh -tt -o EscapeChar=none root@ip -p 23 systemd-tty-ask-password-agent
    ```

    Wait for the passphrase prompt before sending anything, because echo is only off once it appears.

    It will show e.g. `-bash-5.3# `

    Run:

    `systemd-tty-ask-password-agent`

    And type in the LUKS password for that particular machine.

    Wait for disconnection.

## Postflight

1. Keep trying to SSH in over ssh_port, with a 15 second timeout, once every 15 seconds:

    `ssh root@[ipv4 address of machine] -p [ssh_port]`

2. Wait up to 3 minutes for the system to finish booting, running this every 2 seconds until it says neither `initializing` nor `starting`:

    `systemctl is-system-running`

    Meanwhile, if stdout is a terminal, show how many of systemd's jobs (from `systemctl list-jobs`) are waiting, and which are running. If the 3 minutes pass, go on anyway, since a unit can hold startup up for hours (e.g. by retrying a request that a server has rate-limited). Not having finished starting up is then a problem, which names the jobs that were running and counts those that were waiting.

3. Collect this information and show it to the user of `reboop`:

    The state from step 2, and systemd's jobs (if any): `systemctl list-jobs`

    `dmesg -l err,crit,alert,emerg`

    `systemctl --failed`

    Whether we booted into the NixOS configuration we expected

    Whether we booted into the Linux kernel we expected

4. If `/var/lib/reboop/tmux/pending` points to a capture, remove the link (so that a restore cut short isn't repeated on top of itself; the capture stays). If the capture can't be read, or was taken for a reboot or shutdown during this very boot (by its boot id), so that the machine never went down, that's a problem; else bring back what the capture describes:

    Wait up to 10 minutes for every `mount` process to finish, as for filesystems that a script mounts in parallel, which systemd doesn't wait for (just `mount`, which waits for the `mount.*` helper it runs; some helpers stay around as the filesystem's daemon). Go on anyway after that, as a problem.

    For each server whose sessions were listed: unless a server is already running at its socket (then its sessions are added to that one, leaving alone any whose name is taken), start one as its user from a transient unit, `reboop-tmux-USER-TIME`, that logs the user in through PAM: `systemd-run --uid=USER -p PAMName=login --expand-environment=no /bin/sh -c 'mkdir -p SOCKET_DIR && exec tmux -S SOCKET new-session -d -s NAME ...'`. That gives the server the environment that PAM gives a login (on NixOS, PATH and the session variables), and pam_systemd puts it in a logind session scope of the user's (`session-N.scope`), where it stays once the unit is done, like a tmux left behind by logging out. Then, as root through the socket (with `env -i`, so that root's environment doesn't reach what's made: neither the sessions, through update-environment, nor the panes, which tmux gives the PATH of a client that isn't attached; and with `new-session -E`, so that update-environment doesn't unset the server's DISPLAY and such in the sessions), recreate each session (`new-session`, at 400 by 400, so that 200 panes fit, split off one below another, before the layout sizes them; a session in a session group of which a session is back already, or was there already, is created with `-t` that one, sharing its windows, and then selects its own active window), window (`new-window -d -t =SESSION:INDEX -c DIR`, with `-n NAME` unless the name was automatic; the first window of a session by `move-window` to its index) and pane (each after the first split off below the previous one with `split-window -d -f -v -l 2`, so that they come in order; then `select-layout` with the captured layout, which sizes the window and them, and `select-pane` and `select-window` for the active ones; `#` is doubled in directories and names, which tmux expands as formats). A failure with one server or session is a problem, and the rest are still brought back.

    Check each pane: that its process is in the pane's directory (`/proc/PID/cwd` against `readlink -f` of the directory, as a typed command's is as typed, perhaps through a symlink; tmux starts the shell in the home directory if the directory is missing) and that the directory is on the filesystem mounted where it was (`findmnt -T`); a pane that isn't is a problem, and left as it is. Then run its command again if it's resumable: a typed command once the pane's shell is at its prompt (the shell itself is the foreground program, reading in non-canonical mode: `stty -a` on the pane's tty shows `-icanon`; up to 30 seconds), pasted as typed with `set-buffer`, `paste-buffer -p` (bracketed, so that a line break doesn't run what's before it) and `send-keys Enter`, which has zsh-spy record it again; a pane's start command through `respawn-pane -k`, via `source-file -`, since tmux's `#{pane_start_command}` is quoted for tmux's own parser. A pane whose command isn't resumable is left idle and said so. A window linked into several sessions is recreated in the first and linked into the others (`link-window -k`, in place of the window a session was created with, if need be), so that its commands run once.

5. For each of scrub_mounts: if no btrfs filesystem is mounted there (`findmnt --types btrfs --mountpoint MOUNTPOINT`), e.g. because a `nofail` mount failed, that's a problem, and it isn't scrubbed, since a scrub there would scrub the filesystem beneath it instead. Otherwise, unless `btrfs scrub status` says the latest scrub finished without errors, and `/var/lib/reboop/last-clean-scrub/UUID` (the filesystem's) exists and was modified less than scrub_every_days days ago by the machine's clock, run:

    `btrfs scrub start /`

6. In one line, show the user the scrub progress, polled every 2 seconds.

    `btrfs scrub status /`

    `scrub has 3m 30s left, 130.50 GB of 391.56 GB (33.33%) scrubbed at 1.39 GB/s, no errors found`

    If any errors are found, print a very loud warning and return non-0 exit status.

    If the scrub finished without errors, `touch` that file (and make its directory if need be); otherwise remove it, so that the next reboot brings a scrub. Show the scrub's result first, and report a failure to record it as a problem of its own.

## Catch

`reboop catch HOSTNAME` does Reboot step 2 and Postflight for a machine that was rebooted some other way, or whose bounce was cut short. It keeps trying ssh_port, and initrd_ssh_port if there's a stored LUKS password, which it unlocks the initrd with as above. Once the machine accepts SSH on ssh_port (which the initrd doesn't listen on) and `systemctl is-system-running` doesn't say `stopping`, that's the boot it checks.

## Stop

`reboop stop HOSTNAME` does Preflight and Reboot step 1 with `systemctl poweroff` in place of the reboot, then keeps trying ssh_port once every 15 seconds until the machine stops accepting SSH, which is as much of its going down as can be seen from outside.

## Tmux

`reboop tmux list HOSTNAME` lists what Preflight finds in the machine's tmux servers (saving it as a check does). For each server, it prints a line about the server, then a row for each of the server's panes. A server's line counts the server's sessions and panes, or says why the server couldn't be listed (and then no rows follow it). A pane's row has "idle", "resumable" or "not resumable"; the pane's location (`SESSION:WINDOW.PANE`); the pane's directory; and either the command to run again (shortened to a line), why the pane would keep the machine from going down, or (for an idle pane) nothing. The rows of all the servers are laid out as one table, so that their columns line up across the servers. Resumable means by the machine's `resumable_commands`; with `tmux` `block`, a server's line also says that the server blocks a reboot all the same. In color, "idle" is dim, "resumable" is green, and "not resumable" is red, as are a server line's words about the server blocking a reboot or not being listed.

`reboop tmux stop HOSTNAME` refuses if a pane runs something that isn't resumable or a server can't be listed, whatever the machine's `tmux` setting (exit status 2); otherwise saves what's in the servers as `TIME.tmux-stop.json`, points `pending` at it, says what will be run again, and kills each server (`tmux -S SOCKET kill-server`, which hangs up on the panes' processes; one that ignores SIGHUP keeps running), waiting for it to exit. For stopping and starting the servers without a reboot, e.g. around upgrading tmux. (A command typed into a pane after the capture is lost, as a reboot would lose it.)

`reboop tmux start HOSTNAME` does Postflight step 4 (exit status 1 if something couldn't be brought back). It fails if nothing is pending, or what's pending was taken for a reboot or shutdown in the current boot (unlike one taken by `reboop tmux stop`).

`--saved FILE` (a file in `/var/lib/reboop/tmux` by name, or an absolute path on the machine) has `list` show, or `start` bring back, that capture instead: any of them, e.g. the last check's after a crash. A capture from this boot is brought back with it, as told to; sessions whose names are taken are left alone.
