# Model-output: Claude Opus 5.5
#
# The test machine that boots with GRUB from BIOS, like some of the real
# machines: two disks, each with GRUB and a /boot of its own
# (mirroredBoots), and an unencrypted btrfs RAID1 root across both.
{ ... }:

{
  imports = [ ./common.nix ];

  boot.loader.grub = {
    enable = true;
    # Each /boot has its own copy of the kernels, so GRUB needn't read the
    # store off the btrfs RAID1.
    copyKernels = true;
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
