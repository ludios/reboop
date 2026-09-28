# Model-output: Claude Opus 5.5
#
# The test machine that boots with systemd-boot from UEFI, with a
# LUKS-encrypted btrfs root unlocked over SSH from the initrd on port 23.
{ sshKeys, ... }:

{
  imports = [ ./common.nix ];

  boot.loader = {
    systemd-boot.enable = true;
    systemd-boot.configurationLimit = 4;
    # The image builder has no EFI variables to write to; OVMF boots the
    # removable-media path \EFI\BOOT\BOOTX64.EFI that bootctl installs.
    efi.canTouchEfiVariables = false;
  };

  boot.initrd = {
    luks.devices.root = {
      device = "/dev/disk/by-partlabel/root";
      allowDiscards = true;
    };

    network = {
      enable = true;
      ssh = {
        enable = true;
        port = 23;
        # The initrd and stage 2 deliberately have different host keys.
        hostKeys = [ sshKeys.snakeOilPrivateKey ];
        authorizedKeys = [ sshKeys.snakeOilEd25519PublicKey ];
      };
    };
  };

  fileSystems = {
    "/"     = { device = "/dev/mapper/root";           fsType = "btrfs"; options = [ "noatime" "compress=zstd" ]; };
    "/boot" = { device = "/dev/disk/by-partlabel/ESP"; fsType = "vfat";  options = [ "umask=0077" ]; };
  };
}
