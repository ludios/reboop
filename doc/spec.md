<!-- Model-output: Claude Fable 5.1 -->

This is a special-purpose utility for carefully rebooting a NixOS machine, which works in stages.

## General notes

### AI says:

- Set these on every connection attempt: `BatchMode=yes`, `ConnectTimeout=15`, `ServerAliveInterval=5` and `ControlPath=none`
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
    "max_network_transfer_bytes_per_sec": 1000000,
    "max_load_average_1min": 2,
    "root_full_percent": 97,
    "luks_signing_key": "~/.ssh/id_ed25519.pub",
    "stop_services": ["postgresql"],
}
```

`~/.config/reboop/machines.jsonl`

```
{"hostname": "one", "ipv4": "...", "ssh_port": 2222, "stop_services": []}
{"hostname": "two", "ipv4": "...", "scrub_mounts": ["/", "/small"]}
{"hostname": "three", "ipv4": "...", "max_network_transfer_bytes_per_sec": 1000000, "max_load_average_1min": 2}
```

## Preflight

1. SSH in to the machine and get information about these things:

    - Is any btrfs scrub running?
    - Is any btrfs balance or drive replace in progress?
    - Is switch-to-configuration or a nix build running?
    - Any tmux servers running (any user, not just root)?
    - Is an rsync process running?
    - Is a btrfs send or receive running, or cryptsetup?
    - Is any btrfs device missing, or have its error counters (sysfs devinfo/*/error_stats) gone above zero?
    - If the machine has smartctl: what SMART says about each whole disk beneath the btrfs filesystems' devices (`lsblk --inverse` from sysfs' devices/*, then `smartctl -H -A` on all the disks at once, each with a timeout): its overall health, and the counts of sectors gone bad (ATA attributes 5, 187, 197 and 198; for NVMe, the critical warning and media errors)
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

2. Decide whether the machine is okay to reboot.

    - If the boot loader's default doesn't boot the system profile, or its kernel or initrd is missing, or it has the initrd take a static address (ip=) other than the machine's ipv4, no.
    - If any btrfs scrub is running, we can't; Linux will hang on shutdown due to the scrub.
    - If any btrfs balance or drive replace is running, no.
    - If switch-to-configuration or a nix build running, no.
    - If any tmux server running, no.
    - If any rsync process running, no.
    - If a btrfs send or receive, or cryptsetup, is running, no.
    - If a btrfs device is missing (the filesystem wouldn't mount at boot without the degraded option) or has had errors, no.
    - If a disk's SMART self-assessment failed, or any of those counts is above zero, no. (A disk whose SMART smartctl can't read, like a virtual one, doesn't count, but `reboop check` marks it.)
    - If an inhibitor lock asks for no shutdown (mode block or block-weak), no.
    - If a systemd job has lasted 5 seconds, no.
    - If the root filesystem is root_full_percent (97%) used or more, no.
    - If more than 1MB/s being transferred over the network, no.
    - If load average over the last minute > 1, no.

## Reboot

1. SSH in and:

    `systemctl stop` each of stop_services, in order, skipping any the machine doesn't have.

    Check again as in Preflight, except for network traffic and load average, which stopping services changes. If it's no longer okay to reboot, say so, and which stop_services are still stopped, and leave it at that.

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

4. Run:

    `btrfs scrub start /`

5. In one line, show the user the scrub progress, polled every 2 seconds.

    `btrfs scrub status /`

    `scrub has 3m 30s left, 130.50 GB of 391.56 GB (33.33%) scrubbed at 1.39 GB/s, no errors found`

    If any errors are found, print a very loud warning and return non-0 exit status.

## Catch

`reboop catch HOSTNAME` does Reboot step 2 and Postflight for a machine that was rebooted some other way, or whose bounce was cut short. It keeps trying ssh_port, and initrd_ssh_port if there's a stored LUKS password, which it unlocks the initrd with as above. Once the machine is up past its initrd (no /etc/initrd-release), and `systemctl is-system-running` doesn't say `stopping`, that's the boot it checks.

## Stop

`reboop stop HOSTNAME` does Preflight and Reboot step 1 with `systemctl poweroff` in place of the reboot, then keeps trying ssh_port once every 15 seconds until the machine stops accepting SSH, which is as much of its going down as can be seen from outside.
