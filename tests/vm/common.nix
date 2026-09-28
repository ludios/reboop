# Model-output: Claude Opus 5.5
#
# What the NixOS machines that reboop's VM tests run against have in common
# (see systemd-boot.nix and grub.nix for the machines themselves).  They
# resemble the real machines where that matters (a btrfs root, a systemd
# initrd, sshd on port 904, zsh as root's shell) and are otherwise stripped
# down to boot quickly.
#
# `variant` is "base" or "alt", two configurations for tests to switch
# between.  They'd ideally differ in their kernel too, but any second kernel
# means compiling one (and linux 6.12 doesn't build with this GCC anyway).
# `hostname` is the machine's, which default.nix also tells the tests.
{ lib, pkgs, modulesPath, variant, hostname, sshKeys, ... }:

{
  imports = [
    "${modulesPath}/profiles/qemu-guest.nix"
    "${modulesPath}/profiles/minimal.nix"
  ];

  system.stateVersion = "26.05";

  networking.hostName = hostname;

  environment.etc."reboop-test-variant".text = variant;

  # The kernel the real machines run, so it's already built.
  boot.kernelPackages = pkgs.linuxPackages_6_18;

  boot.loader.timeout = 0;

  boot.kernelParams = [ "console=ttyS0" ];

  boot.initrd.systemd.enable = true;

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
  ];

  services.timesyncd.enable = false;
  services.logrotate.enable = false;
  nix.settings.substituters = lib.mkForce [ ];
}
