<!-- Model-output: Claude Opus 5.5 -->

# reboop

Reboots NixOS machines over SSH, carefully. `reboop bounce HOSTNAME`:

1. Refuses if the machine is busy: a btrfs scrub (which would hang shutdown), balance or other exclusive operation; a Nix command or build, or switch-to-configuration; a tmux server; rsync; network traffic or load average over a limit. `reboop check` shows this for all machines without rebooting any.
2. If / is on LUKS, checks that the stored password still opens it.
3. Stops PostgreSQL and reboots.
4. Unlocks LUKS (if any) by SSHing into the systemd initrd.
5. Once booted, shows kernel errors, failed units, and whether the expected NixOS system and kernel came up.
6. Scrubs btrfs, failing loudly on errors.

LUKS passwords are stored in `~/.config/reboop/luks/`, encrypted with a key derived from an `ssh-keygen -Y sign` signature, so they're usable wherever your Ed25519 or RSA SSH key is. Machines and limits go in `~/.config/reboop/defaults.json` and `machines.jsonl`; see [doc/spec.md](doc/spec.md).
