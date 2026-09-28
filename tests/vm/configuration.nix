# Model-output: Claude Opus 5.5
#
# The NixOS machine that reboop's VM tests run against.  It resembles the real
# machines where that matters (a LUKS-encrypted btrfs root unlocked over SSH
# from a systemd initrd on port 23, sshd on port 904, zsh as root's shell) and
# is otherwise stripped down to boot quickly.
#
# `variant` is "base" or "alt", two configurations for tests to switch
# between.  They'd ideally differ in their kernel too, but any second kernel
# means compiling one (and linux 6.12 doesn't build with this GCC anyway).
{ lib, pkgs, modulesPath, variant, sshKeys, ... }:

{
  imports = [
    "${modulesPath}/profiles/qemu-guest.nix"
    "${modulesPath}/profiles/minimal.nix"
  ];

  system.stateVersion = "26.05";

  networking.hostName = "reboop-test";

  environment.etc."reboop-test-variant".text = variant;

  # The kernel the real machines run, so it's already built.
  boot.kernelPackages = pkgs.linuxPackages_6_18;

  boot.loader = {
    systemd-boot.enable = true;
    systemd-boot.configurationLimit = 4;
    # The image builder has no EFI variables to write to; OVMF boots the
    # removable-media path \EFI\BOOT\BOOTX64.EFI that bootctl installs.
    efi.canTouchEfiVariables = false;
    timeout = 0;
  };

  boot.kernelParams = [ "console=ttyS0" ];

  boot.initrd = {
    systemd.enable = true;

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

  networking = {
    useNetworkd = true;
    useDHCP = true;
    firewall.enable = false;
  };
  systemd.network.wait-online.enable = false;

  services.openssh = {
    enable = true;
    ports = [ 904 ];
    settings = {
      PasswordAuthentication = false;
      KbdInteractiveAuthentication = false;
    };
    hostKeys = [ { type = "ed25519"; path = "/etc/ssh/ssh_host_ed25519_key"; } ];
  };
  environment.etc."ssh/ssh_host_ed25519_key" = {
    source = sshKeys.snakeOilEd25519PrivateKey;
    mode = "0600";
  };

  programs.zsh.enable = true;
  users.defaultUserShell = pkgs.zsh;
  users.users.root.openssh.authorizedKeys.keys = [ sshKeys.snakeOilEd25519PublicKey ];

  # An unprivileged user for tests that need processes owned by someone else.
  users.users.tester = {
    isNormalUser = true;
    uid = 1000;
  };

  environment.systemPackages = with pkgs; [
    tmux
    rsync
    lvm2 # for dmsetup
  ];

  # For tests that slow down a block device.
  boot.kernelModules = [ "dm_delay" ];

  services.timesyncd.enable = false;
  services.logrotate.enable = false;
  nix.settings.substituters = lib.mkForce [ ];
}
