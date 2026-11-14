# reboop

Reboots NixOS machines over SSH, carefully:

- It refuses to reboot while its checks find work that a reboot would interrupt, or a problem with the next boot.
- It unlocks LUKS from the initrd, and shows how the machine came back.
- It can bring back what was running in tmux, and it scrubs btrfs.

[doc/spec.md](doc/spec.md) has the details that this README leaves out.

## Commands

`HOSTNAME` is a machine's `hostname` in `machines.jsonl`. Each command's `--help` gives its options. A command exits with status 0 if all went well, 2 if a machine wasn't okay to reboot (or to shut down, or to have its tmux servers stopped), or 1 for any other problem.

| Command | What it does |
| --- | --- |
| `reboop check [HOSTNAME...]` | For each machine (by default, all of them): whether it's okay to reboot, and why it needs a reboot, if it does. Reboots nothing. |
| `reboop bounce HOSTNAME` | Reboots the machine if it's okay to, and sees it back up: see [What `bounce` does](#what-bounce-does). |
| `reboop catch HOSTNAME` | Steps 4 to 7 of `bounce`, for a machine that was rebooted some other way (by hand, or by a power cut), or whose bounce was cut short. If the machine is already up, checks the boot it's in. |
| `reboop stop HOSTNAME` | Steps 1 and 3 of `bounce`, but powers the machine off (`systemctl poweroff`) instead of rebooting it, then waits until it stops accepting SSH. |
| `reboop tmux list HOSTNAME`<br>`reboop tmux stop HOSTNAME`<br>`reboop tmux start HOSTNAME` | Shows, saves and kills, or brings back what's in the machine's tmux servers: see [tmux](#tmux). |
| `reboop set-luks-password HOSTNAME` | Stores the machine's LUKS password: see [Setup](#setup). |
| `reboop get-luks-password HOSTNAME` | Prints the machine's stored LUKS password. |

## What `bounce` does

1. Checks that the machine is okay to reboot: see [What makes reboop refuse](#what-makes-reboop-refuse).
2. If there's a LUKS device beneath `/` (only one is supported), checks that:
   - the stored password still opens it;
   - if the initrd takes static addresses (`ip=`), one of them is the machine's `ipv4`.
3. Stops the machine's `stop_services` (e.g. PostgreSQL), in order, each along with the active units that would start it again (its `TriggeredBy=`, such as socket and timer units). Then it checks again (apart from load and network traffic, which stopping services changes), saves what's in tmux, and reboots.
   - If a `systemctl stop` fails, or the machine is no longer okay to reboot, reboop doesn't reboot. It lists the units that it stopped, and leaves them stopped.
4. Unlocks LUKS, if there is any, by SSHing into the systemd initrd.
5. Once it can SSH into the machine, waits up to 3 minutes for systemd to finish starting, then shows:
   - kernel errors and failed units;
   - whether the expected NixOS system and kernel came up.
6. Brings back what was in tmux, if a capture is pending: see [tmux](#tmux).
7. Scrubs btrfs, if a scrub is due, which can take hours: see [Scrubs](#scrubs).

## What makes reboop refuse

`reboop check` shows these reasons for every machine, and `bounce` and `stop` refuse if there are any. [doc/spec.md](doc/spec.md#preflight) lists the programs and rules in full.

- **Boot loader**: there's no default entry (systemd-boot's, or GRUB's in every mirrored /boot), or it wouldn't boot the system profile, or a kernel or initrd file that it names is missing. Or the UEFI firmware has a one-time boot entry set (`BootNext`, whatever it points to), or is set to open its setup.
- **btrfs**: a scrub (which would hang shutdown), a balance or other exclusive operation, a missing device, or a device that has had errors.
- **Disks**: SMART reports a problem with a disk beneath btrfs, or doesn't answer in time. This is checked only where smartmontools is installed, and a disk in standby isn't woken to check it.
- **Work in progress**:
  - a Nix command or build, or switch-to-configuration;
  - a copy or transfer (e.g. rsync, cp, dd, `rclone copy`, or a btrfs send or receive);
  - cryptsetup, or a disk tool (e.g. fsck, mkfs, or a partitioner);
  - a tmux server, unless the machine's `tmux` setting is `resume` and reboop can bring back everything in it: see [tmux](#tmux);
  - an inhibitor lock that blocks shutdown (see `systemd-inhibit`), or a systemd job that lasts through a 5-second sample.
- **Limits**: network traffic over `max_network_transfer_bytes_per_sec`, a 1-minute load average over `max_load_average_1min`, or a root filesystem that's `root_full_percent` (97% by default) full or more.

## What else `reboop check` shows

Why a machine needs a reboot, if it does:

- The system profile isn't the active configuration (as after `nixos-rebuild boot`).
- The profile has a new or rebuilt kernel, kernel modules or initrd, compared with what booted.
- The profile has a new or rebuilt systemd, compared with the one that PID 1 runs.

Whether `nixos-rebuild switch` or `boot` would change the system profile. `bounce` warns if so, but goes ahead.

- reboop finds that out by evaluating the toplevel of `<nixpkgs/nixos>`, with `nixos-config` from the machine's NIX_PATH (`nix.nixPath`). For a machine whose configuration is a flake, this evaluation fails (shown as `?`), which is no reason to refuse.
- The evaluation builds nothing, but it downloads the sources that the configuration fetches, like a rebuild would, and it can take a GB or more of the machine's RAM.
- `"check_rebuild": false` turns it off.

`reboop check -v`, or naming machines (`reboop check HOSTNAME`), prints the details of each below the table.

## tmux

With the default `"tmux": "block"`, any tmux server makes reboop refuse.

With `"tmux": "resume"`, right before taking the machine down, reboop saves what's in its tmux servers, and brings that back once the machine is up:

- every session, window and pane, with the windows' names and layouts;
- the directory where each pane's command was typed or started (or, for an idle pane, its current directory);
- each pane's command, if one of the machine's `resumable_commands` matches it. Idle panes come back idle.

reboop learns which command line was typed into a pane's zsh by reading the archive that this zsh holds open, so the zsh must load [zsh-spy](https://github.com/ludios/zsh-spy).

### `resumable_commands`

- Each one is a regular expression (in the syntax of Rust's regex crate) that has to match the whole command line: `"yt-dlp .*"`, not `"yt-dlp"`.
- A machine's own `resumable_commands` are added to those in defaults.json.
- A pane started with a command rather than a shell (`tmux new-window 'sleep 600'`) is matched as tmux quotes that command: `"sleep 600"`, with the quotes.

A command runs again from the start, without what was set by hand in the old shell (like variables and functions), and without the pane's scrollback. So only commands that can start over belong in `resumable_commands`.

### What makes reboop refuse, even with `resume`

- A pane running a command that no pattern matches, or that's a reason to refuse in its own right, like a copy.
- A pane whose command reboop can't know, for example because the shell has no zsh-spy archive, or because HIST_IGNORE_SPACE hid the line.
- A pane whose shell has a job that hasn't ended, in the background or stopped with Ctrl-Z. ([doc/spec.md](doc/spec.md#preflight) describes the one exception, for a job brought back with `fg`.)
- A pane whose command's directory is gone.
- A server that can't be listed, as when its tmux is another version than the machine's, or it's in a container's network namespace.

### Saved captures

- Each time reboop looks at the servers (for `check`, `tmux list`, `tmux stop`, or a `bounce` or `stop` that gets as far as taking the machine down), it saves a capture of them on the machine in `/var/lib/reboop/tmux/`. Only root can read the captures, as they hold users' command lines.
- `bounce`, `stop` and `tmux stop` mark their capture as pending, if there's anything in it, for step 6 of `bounce` (or `catch`, or `tmux start`) to bring back, whatever the `tmux` setting.

### `reboop tmux`

- `list HOSTNAME` shows each pane's directory and command, and whether it's resumable.
- `stop HOSTNAME` saves what's in the servers and kills them (their processes get SIGHUP), for `start` to bring back: e.g. around upgrading tmux. It refuses if a pane is neither idle nor resumable, or if a server can't be listed, whatever the `tmux` setting.
- `start HOSTNAME` brings back the pending capture.
- `--saved FILE` makes `list` show, or `start` bring back, another capture saved on the machine: e.g. after a restore was cut short (which isn't repeated by itself), or the last check's after a crash.

## Scrubs

After the reboot, reboop scrubs each btrfs filesystem mounted at one of `scrub_mounts`, and waits for the scrub to end. It shows the progress, and fails loudly on errors. It skips a filesystem if both:

- `btrfs scrub status` says that the latest scrub finished clean;
- reboop recorded a clean scrub of it less than `scrub_every_days` (14 by default) days ago.

reboop records a clean scrub as the modification time of `/var/lib/reboop/last-clean-scrub/UUID` (named by the filesystem's UUID) on the machine. A scrub that isn't clean removes the file.

- Delete the file to get a scrub at the next `bounce` or `catch`.
- Touch it to put the next scrub off: `scrub_every_days` then counts from now.

## Setup

- **Install**: `cargo install --locked --path .` in this repository.
- **Machines and limits**: in `~/.config/reboop/` (or `$XDG_CONFIG_HOME/reboop/`). [doc/spec.md](doc/spec.md#configuration-file) has examples.
  - `defaults.json` (JSON5) has the settings for every machine.
  - `machines.jsonl` has a JSON object per line: a machine's `hostname` (the name that the machine calls itself), its `ipv4`, and any settings that replace the defaults. (`resumable_commands` are added to the defaults' instead.)
  - `ssh_port`, `initrd_ssh_port`, `scrub_mounts`, `max_network_transfer_bytes_per_sec` and `max_load_average_1min` have no built-in default, so each must be set in one of the two files.
- **SSH**: reboop logs in to the machine as root with your `ssh` program, without prompting (`BatchMode=yes`), and never accepts a new host key, whatever your ssh config says. known_hosts must already have these keys, normally under `ipv4` (or `[ipv4]:port` for a port other than 22):
  - the machine's, at its `ssh_port`;
  - if there's LUKS, the initrd's, at its `initrd_ssh_port`. The machine's systemd initrd must have networking and an SSH server that lets root in.
- **LUKS passwords**: `reboop set-luks-password HOSTNAME` asks for the password, checks over SSH that it opens the LUKS device beneath `/`, and stores it in `~/.config/reboop/luks/`.
  - It's encrypted with a key derived from an `ssh-keygen -Y sign` signature, made with the key of yours that `luks_signing_key` names (`~/.ssh/id_ed25519.pub` by default): an Ed25519 or RSA private key file, or a public key file whose private key is in ssh-agent.
  - So the password files can be decrypted on any computer that can sign with that key.

## Contributing

Well-conceived pull requests by humans or frontier LLMs are welcome. Please provide the motivation and some evidence that it solved something, when possible. 
