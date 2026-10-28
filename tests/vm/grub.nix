# Model-output: Claude Opus 5.5
#
# The test machine that boots with GRUB from BIOS, like some of the real
# machines: two disks, each with GRUB and a /boot of its own
# (mirroredBoots), and an unencrypted btrfs RAID1 root across both.
{ ... }:

{
  imports = [ ./common.nix ];

  # systemd's 64-btrfs.rules marks each member of a multi-device btrfs not
  # ready until the kernel has seen them all, and the member that completes
  # the filesystem then retriggers those that udev's database lists as not
  # ready.  A member whose worker hasn't written the database yet is missed
  # and stays not ready, and as the member processed last, it can hold
  # /dev/disk/by-label/root, so that the initrd waits for the root until it
  # gives up, in roughly 1 boot of 300 here (see
  # https://github.com/systemd/systemd/issues/41552).  So leave the link to
  # a ready member, until systemd's rules do.
  boot.initrd.services.udev.rules = ''
    ENV{ID_BTRFS_READY}=="0", OPTIONS+="link_priority=-100"
  '';

  # With a separate /boot, NixOS copies the kernels there.
  boot.loader.grub = {
    enable = true;
    mirroredBoots = [
      { devices = [ "/dev/vda" ]; path = "/boot"; }
      { devices = [ "/dev/vdb" ]; path = "/boot-fallback"; }
    ];
  };

  fileSystems = {
    "/"              = { device = "/dev/disk/by-label/root";      fsType = "btrfs"; options = [ "noatime" "compress=zstd" ]; };
    "/boot"          = { device = "/dev/disk/by-partlabel/boot-1"; fsType = "ext4"; };
    "/boot-fallback" = { device = "/dev/disk/by-partlabel/boot-2"; fsType = "ext4"; };
  };
}
