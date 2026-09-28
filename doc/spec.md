This is a special-purpose utility for carefully rebooting a NixOS machine, which works in stages.

## General notes

AI says:

    - Set these on every connection attempt: BatchMode=yes, ConnectTimeout=15, ServerAliveInterval=5 and ControlPath=none
        - ControlPath=none matters if your ssh config uses ControlMaster/ControlPersist: a post-reboot attempt can otherwise reuse the dead shared connection and hang.
        - The keepalive matters because the initrd can drop the network without cleanly closing the connection.
        - Give every wait loop an overall deadline

    - Use the ssh binary rather than a Rust SSH library, so our ssh config, agent and known_hosts all just work.

Managing LUKS keys:

    printf reboop-luks-v1 | ssh-keygen -Y sign -n reboop-luks -f ~/.ssh/id_ed25519.pub

    and hash the signature into a 256-bit master key;

    use that master key with XChaCha20-Poly1305 to encrypt / decrypt files storing LUKS passwords in:

    ~/.config/reboop/luks/HOSTNAME

## Configuration file

~/.config/reboop/defaults.json (use JSON5 or JSONC, whatever's better for Rust)

{
    "ssh_port": 904,
    "initrd_ssh_port": 23,
    "scrub_mounts": ["/"],
    "max_network_transfer_bytes_per_sec": 1000000,
    "max_load_average_1min": 2,
}

~/.config/reboop/machines.jsonl

{"hostname": "one", "ipv4": "...", "ssh_port": 22, "initrd_ssh_port": 23}
{"hostname": "two", "ipv4": "...", "scrub_mounts": ["/", "/small"]}
{"hostname": "three", "ipv4": "...", "max_network_transfer_bytes_per_sec": 1000000, "max_load_average_1min": 2}

## Preflight

1. SSH in to the machine and get information about these things:

    - Is any btrfs scrub running?
    - Is any btrfs balance or drive replace in progress?
    - Is switch-to-configuration or a nix build running?
    - Any tmux servers running (any user, not just root)?
    - Is an rsync process running?
    - Network bytes in / out over 5 seconds
    - Load average over the last minute
    - The NixOS configuration we're currently on
    - The Linux kernel we're currenty on
    - The NixOS configuration and kernel we expect to boot into by default

2. Decide whether the machine is okay to reboot.

    - If any btrfs scrub is running, we can't; Linux will hang on shutdown due to the scrub.
    - If any btrfs balance or drive replace is running, no.
    - If switch-to-configuration or a nix build running, no.
    - If any tmux server running, no.
    - If any rsync process running, no.
    - If more than 1MB/s being transferred over the network, no.
    - If load average over the last minute > 1, no.

## Reboot

1. SSH in and:

    - systemctl stop postgresql
    - shutdown -r now

2. After disconnection, keep trying to SSH in over port 23, with a 15 second timeout, once every 15 seconds:

    - ssh root@[ipv4 address of machine] -p 23

    until success.

    Or (AI says):
    
        ssh -tt -o EscapeChar=none root@ip -p 23 systemd-tty-ask-password-agent

        Wait for the passphrase prompt before sending anything, because echo is only off once it appears.

    It will show e.g. "-bash-5.3# "

    Run:

    - systemd-tty-ask-password-agent

    And type in the LUKS password for that particular machine.

    Wait for disconnection.

## Postflight

1. Keep trying to SSH in over port 904, with a 15 second timeout, once every 15 seconds:

    ssh root@[ipv4 address of machine] -p 904

2. Wait for the system to finish booting:

    systemctl is-system-running --wait

3. Collect this information and show it to the user of `reboop`:

    dmesg -l err,crit,alert,emerg

    systemctl --failed

    Whether we booted into the NixOS configuration we expected

    Whether we booted into the Linux kernel we expected

4. Run:

    btrfs scrub start /

5. In one line, show the user the scrub progress, polled every 2 seconds.

    btrfs scrub status /

    scrub has 3m 30s left, 130.50GB of 391.56GB (33.33%) scrubbed at 1.39GB/s, no errors found

    If any errors are found, print a very loud warning and return non-0 exit status.
