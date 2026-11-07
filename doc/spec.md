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

A machine's settings replace the defaults, except `resumable_commands`, which are added to the defaults'. Each is a regular expression that has to match a whole command line (so `yt-dlp .*`, not `yt-dlp`; `(?s)` lets `.` match line breaks). `tmux` is `block` (any tmux server keeps the machine from going down) unless set to `resume` (see Tmux below).

## Preflight

1. SSH in to the machine and get information about these things:

    - Is any btrfs scrub running?
    - Is any btrfs balance or drive replace in progress?
    - Is switch-to-configuration or a nix build running?
    - Every tmux server (any user's, at any socket: `ss -xlpH` lists the sockets with their processes), and what's in it, with `tmux -S SOCKET list-panes -a` as root: each session, window (index; name, unless automatic-rename is on; layout; size; whether active) and pane (index, whether active, current directory), and what the pane is running: the command line typed into its shell, from the zsh-spy archive (hist.*.jsonl) that the shell holds open, found through /proc/PID/fd, as the last command_start record without a command_end; or the command tmux started the pane with (`#{pane_start_command}`); or the background jobs the shell started that haven't ended (async_start without an async_end or async_lost); or idle; or unknown, when a program is in the foreground but its shell has no archive or the line was typed with a leading space. Also where the filesystem that the pane's directory is on is mounted (`findmnt -T`), for checking it's back. A server whose panes can't be listed (e.g. its tmux is another version) is noted as such.
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
    - After the rest, if check_rebuild is on: whether `nixos-rebuild switch` or `boot` would change the system profile, i.e. whether the configuration evaluates to another toplevel than the profile's: `nix-instantiate --eval --raw --option allow-import-from-derivation false -A config.system.build.toplevel.outPath '<nixpkgs/nixos>'`, niced, as the OOM killer's first pick (`choom -n 1000`), within 3 minutes. Nothing is built (a configuration that imports from a derivation fails), though sources the configuration fetches (e.g. with `builtins.fetchTarball`) get downloaded into the store. Only that default way of building is supported: if nixos-rebuild would build /etc/nixos/flake.nix, `<nixos-system>` or /etc/nixos/system.nix instead, or looking for `<nixos-system>` fails (within 30 seconds), or NIX_PATH is empty in the session, it's a failure, as is an evaluation that fails. `reboop check` shows "ok", "stale", "?" for a failure, or nothing if it's off, and says what it would change or why it failed below the table; `reboop bounce` warns about either but goes ahead. It's no reason not to reboot.

2. Decide whether the machine is okay to reboot.

    - If the boot loader's default doesn't boot the system profile, or its kernel or initrd is missing, or it has the initrd take a static address (ip=) other than the machine's ipv4, no.
    - If any btrfs scrub is running, we can't; Linux will hang on shutdown due to the scrub.
    - If any btrfs balance or drive replace is running, no.
    - If switch-to-configuration or a nix build running, no.
    - If any tmux server is running and the machine's `tmux` is `block`, no. If it's `resume`: no if a pane runs a command that none of the machine's `resumable_commands` matches as a whole, or what a pane runs is unknown, or a pane's shell has a background job still running, or a server couldn't be listed; idle panes and resumable commands are fine, to be brought back after the reboot (see Tmux below).
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

    `systemctl stop` each of stop_services, in order, skipping any the machine doesn't have.

    Check again as in Preflight, except for network traffic and load average, which stopping services changes. If it's no longer okay to reboot, say so, and which stop_services are still stopped, and leave it at that.

    Save what that check found in the tmux servers on the machine, as `/var/lib/reboop/tmux/TIME.reboot.json` (`.shutdown.json` for a stop), and if there's anything in them, point `/var/lib/reboop/tmux/pending` at it, and say what will be run again. (Every `reboop check` and `reboop tmux list` saves a `TIME.check.json` there too, without `pending`; the last 200 captures are kept, for root only.)

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

2. Wait for the system to finish booting:

    `systemctl is-system-running --wait`

3. Collect this information and show it to the user of `reboop`:

    `dmesg -l err,crit,alert,emerg`

    `systemctl --failed`

    Whether we booted into the NixOS configuration we expected

    Whether we booted into the Linux kernel we expected

4. If `/var/lib/reboop/tmux/pending` points to a capture, remove the link (so that a restore cut short isn't repeated on top of itself; the capture stays), and bring back what the capture describes, unless it's from this very boot (by its boot id) and was for a reboot or shutdown, in which case the machine never went down and what it describes is still running:

    Wait up to 10 minutes for every `mount` and `mount.*` process to finish, as for filesystems that a script mounts in parallel, which systemd doesn't wait for.

    For each server whose sessions were listed: unless a server is already running at its socket (then its sessions are added to that one, leaving alone any whose name is taken), start one as its user, inside a logind session of theirs (the PATH and the rest of the environment of a login), in a transient unit, `reboop-tmux-USER-TIME`, that lasts as long as the server: `systemd-run --uid=USER -p PAMName=login -p ExitType=cgroup -p CollectMode=inactive-or-failed /bin/sh -c 'mkdir -p SOCKET_DIR && exec tmux -S SOCKET new-session -d -s NAME ...'`, the session at the largest of its windows' sizes. Then, as root through the socket, recreate each session (`new-session`), window (`new-window -d -t =SESSION:INDEX -c DIR`, with `-n NAME` unless the name was automatic; the first window of a session by `move-window` to its index) and pane (each after the first split off below the previous one with `split-window -d -f -v -l 2`, so that they come in order; then `select-layout` with the captured layout, which sizes them, and `select-pane` and `select-window` for the active ones; `#` is doubled in directories and names, which tmux expands as formats).

    Run each pane's command again if it's resumable, once the pane's shell is at its prompt (`stty -a` on the pane's tty shows `-icanon`; up to 30 seconds), is in the pane's directory (`/proc/PID/cwd`; tmux starts the shell in the home directory if the directory is missing) and the directory is on the filesystem mounted where it was (`findmnt -T`): a typed command is pasted as typed with `set-buffer`, `paste-buffer -p` (bracketed, so that a line break doesn't run what's before it) and `send-keys Enter`, which has zsh-spy record it again; a pane's start command runs through `respawn-pane -k`, via `source-file -`, since tmux's `#{pane_start_command}` is quoted for tmux's own parser. A pane whose command isn't resumable is left idle and said so; one whose shell isn't ready, isn't where it should be, or whose directory isn't on the right filesystem is left idle as a problem.

5. Unless `btrfs scrub status` says the latest scrub finished without errors, and `/var/lib/reboop/last-clean-scrub/UUID` (the filesystem's) exists and was modified less than scrub_every_days days ago by the machine's clock, run:

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

`reboop tmux list HOSTNAME` lists what Preflight finds in the machine's tmux servers (saving it as a check does): a line per server, then one per pane with its location (`SESSION:WINDOW.PANE`), directory, and what it runs, as "idle", "COMMAND (resumable)", or why it would keep the machine from going down.

`reboop tmux stop HOSTNAME` refuses, as a reboot would, if a pane runs something that isn't resumable (exit status 2); otherwise saves what's in the servers as `TIME.tmux-stop.json`, points `pending` at it, says what will be run again, and kills each server (`tmux -S SOCKET kill-server`, which hangs up on the panes' processes), waiting for it to exit. For stopping and starting the servers without a reboot, e.g. around upgrading tmux.

`reboop tmux start HOSTNAME` does Postflight step 4 (exit status 1 if something couldn't be brought back). It fails if nothing is pending.

`--saved FILE` (a file in `/var/lib/reboop/tmux` by name, or any path on the machine) has `list` show, or `start` bring back, that capture instead: any of them, e.g. the last check's after a crash. A capture from this boot for a reboot is brought back with it, as told to; sessions whose names are taken are left alone.
