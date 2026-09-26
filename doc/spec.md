This is a special-purpose utility for carefully rebooting a NixOS machine, which works in stages.

## Preflight

1. SSH in to the machine and get information about these things:

    - Is any btrfs scrub running?
    - Any tmux servers running?
    - Is an rsync process running?
    - Network bytes in / out over 5 seconds
    - Load average over the last minute
    - The IPv4 IP address (WAN) of the machine

2. Decide whether the machine is okay to reboot.

    - If any btrfs scrub is running, we can't; Linux will hang on shutdown due to the scrub.
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

    It will show e.g. "-bash-5.3# "

    Run:

    - systemd-tty-ask-password-agent

    And type in the LUKS password for that particular machine.

    Wait for disconnection.

## Postflight

1. Keep trying to SSH in over port 904, with a 15 second timeout, once every 15 seconds:

    ssh root@[ipv4 address of machine] -p 904

2. Collect this information and show it to the user of `reboop`:

    dmesg -l err,crit,alert,emerg

3. Run:
    
    fast-nix-gc --delete-older-than 14d

4. Run:

    btrfs scrub start /

5. In one line, show the user the scrub progress, polled every 2 seconds.

    btrfs scrub status /

    scrub has 3m 30s left, 130.50GB of 391.56GB (33.33%) scrubbed at 1.39GB/s, no errors found

    If any errors are found, print a very loud warning and return non-0 exit status.
