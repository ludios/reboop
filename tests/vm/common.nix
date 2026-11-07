# What the NixOS machines that reboop's VM tests run against have in common
# (see systemd-boot.nix and grub.nix for the machines themselves).  They
# resemble the real machines where that matters (a btrfs root, a systemd
# initrd, which is nixpkgs's only kind since 26.11, zsh as root's shell) and
# are otherwise stripped down to boot quickly.
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
    ./btrfs-ready-recheck.nix
  ];

  system.stateVersion = "26.05";

  networking.hostName = hostname;

  environment.etc."reboop-test-variant".text = variant;

  # The kernel the real machines run, so it's already built.
  boot.kernelPackages = pkgs.linuxPackages_6_18;

  boot.loader.timeout = 0;

  boot.kernelParams = [ "console=ttyS0" ];

  networking = {
    useNetworkd = true;
    useDHCP = true;
    firewall.enable = false;
  };
  systemd.network.wait-online.enable = false;

  services.openssh = {
    enable = true;
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

  # zsh-spy, which reboop reads to tell what the panes of a tmux server are
  # running, as on the real machines; and hist_ignore_space, which hides
  # lines typed with a leading space from it.
  programs.zsh.interactiveShellInit = ''
    setopt hist_ignore_space
    source ${pkgs.fetchFromGitHub {
      owner = "ludios";
      repo = "zsh-spy";
      rev = "d6b8a3012b20e5d3e2d8532f24e86a5c5af9edbf";
      hash = "sha256-ESW3tzkmkdhXSnDlmRJlSDp6n2Elm+dTfE+LyTFSkDA=";
    }}/zsh-spy.zsh
  '';

  environment.systemPackages = with pkgs; [
    tmux
    rsync
    smartmontools
  ];

  # A stub nixpkgs, so that evaluating the configuration (see facts::rebuild)
  # is cheap, and gives whatever toplevel a test writes to the file (without
  # a trailing newline).  NIX_PATH reaches reboop's session the way it does
  # on the real machines, which set nix.nixPath too.
  nix.nixPath = [ "nixpkgs=/etc/reboop-test-nixpkgs" "nixos-config=/etc/nixos/configuration.nix" ];
  environment.etc."reboop-test-nixpkgs/nixos/default.nix".text = ''
    { config.system.build.toplevel.outPath = builtins.readFile /var/tmp/reboop-test-toplevel; }
  '';

  services.timesyncd.enable = false;
  services.logrotate.enable = false;
  nix.settings.substituters = lib.mkForce [ ];
}
