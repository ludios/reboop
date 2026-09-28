<!-- Model-output: Claude Opus 5.5 -->

# reboop

Reboots NixOS machines over SSH, carefully. For each machine:

1. Refuse if it's busy: a btrfs scrub (which would hang shutdown), balance or device replace; a nix build or switch-to-configuration; a tmux server; rsync; network traffic or load average over a limit.
2. Stop PostgreSQL and reboot.
3. Unlock LUKS by SSHing into the initrd.
4. Once booted, show kernel errors, failed units, and whether the expected NixOS system and kernel came up.
5. Scrub btrfs, failing loudly on errors.

LUKS passwords are stored in `~/.config/reboop/luks/`, encrypted with a key derived from an `ssh-keygen -Y sign` signature, so they're usable wherever your SSH key is. Machines and limits go in `~/.config/reboop/defaults.json` and `machines.jsonl`; see [doc/spec.md](doc/spec.md).
