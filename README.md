<!-- Model-output: Claude Opus 5.5 -->

# reboop

Reboots NixOS machines over SSH, carefully. `reboop bounce HOSTNAME`:

1. Refuses if the boot loader's default (systemd-boot's, or GRUB's in every mirrored /boot) wouldn't boot the system profile, lacks its kernel or initrd, or has the initrd take a static address (`ip=`) other than the machine's. Refuses if the machine is busy: a btrfs scrub (which would hang shutdown), balance or other exclusive operation, or a device that's missing or has had errors; a Nix command or build, or switch-to-configuration; a tmux server; rsync; a btrfs send or receive; cryptsetup; an inhibitor lock against shutdown (see `systemd-inhibit`); a systemd job that lasts 5 seconds or more; network traffic or load average over a limit; a root filesystem that's nearly full (97% by default). `reboop check` shows this for all machines without rebooting any.
2. If / is on LUKS, checks that the stored password still opens it.
3. Stops the machine's `stop_services` (e.g. PostgreSQL), checks again that it's okay to reboot (apart from load and network traffic, which stopping services changes), and reboots. If it's no longer okay, it leaves the services stopped and says so.
4. Unlocks LUKS (if any) by SSHing into the systemd initrd.
5. Once booted, shows kernel errors, failed units, and whether the expected NixOS system and kernel came up.
6. Scrubs btrfs, failing loudly on errors.

`reboop catch HOSTNAME` does steps 4 to 6 for a machine that was rebooted some other way, like by hand or by a power cut, or whose bounce was cut short. If the machine is already up, it checks the boot it's in.

LUKS passwords are stored in `~/.config/reboop/luks/`, encrypted with a key derived from an `ssh-keygen -Y sign` signature, so they're usable wherever your Ed25519 or RSA SSH key is. Machines and limits go in `~/.config/reboop/defaults.json` and `machines.jsonl`; see [doc/spec.md](doc/spec.md).

## Contributing

Well-conceived pull requests by humans or frontier LLMs are welcome. Please provide the motivation and some evidence that it solved something, when possible. 
