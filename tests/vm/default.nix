# Model-output: Claude Opus 5.5
#
# Everything each test VM needs, gathered into a directory per VM:
#
#     nix-build tests/vm -A systemd-boot
#     nix-build tests/vm -A grub
#
# See manifest.json in the output for what's where.
{ nixpkgs ? <nixpkgs> }:

let
  pkgs = import nixpkgs { };
  qemuCommon = import "${nixpkgs}/nixos/lib/qemu-common.nix" { inherit (pkgs) lib stdenv; };
  qemuBinary = qemuCommon.qemuBinary pkgs.qemu_test;

  # Test-only keys from RFC 9500, shipped with nixpkgs for use in VM tests.
  sshKeys = import "${nixpkgs}/nixos/tests/ssh-keys.nix" pkgs;

  # The NixOS system built from `configuration` for the machine called
  # `hostname`, in the "base" or "alt" `variant`.
  mkSystem = configuration: hostname: variant:
    (import "${nixpkgs}/nixos/lib/eval-config.nix" {
      system = pkgs.stdenv.hostPlatform.system;
      modules = [
        configuration
        { _module.args = { inherit variant hostname sshKeys; }; }
      ];
    }).config.system.build.toplevel;

  # A directory with the VM's manifest.json, which has `fields` and what
  # every VM has.
  bundle = name: fields: pkgs.runCommand "reboop-test-vm-${name}" { } ''
    mkdir $out
    ln -s ${pkgs.writeText "manifest.json" (builtins.toJSON ({
      qemu         = "${pkgs.qemu_test}/bin/qemu-system-x86_64";
      client_key   = "${sshKeys.snakeOilEd25519PrivateKey}";
      host_key_pub = sshKeys.snakeOilEd25519PublicKey;
    } // fields))} $out/manifest.json
  '';
in
{
  systemd-boot =
    let
      hostname = "reboop-test";
      base = mkSystem ./systemd-boot.nix hostname "base";
      alt  = mkSystem ./systemd-boot.nix hostname "alt";
      lukspassword = "reboop test passphrase";
      image = import ./image-systemd-boot.nix { inherit pkgs qemuBinary base alt lukspassword; };
    in
    bundle "systemd-boot" {
      inherit hostname;
      disk_images    = [ "${image}/disk.qcow2" ];
      ovmf           = { code = "${pkgs.OVMF.fd}/FV/OVMF_CODE.fd"; vars = "${pkgs.OVMF.fd}/FV/OVMF_VARS.fd"; };
      initrd_key_pub = sshKeys.snakeOilPublicKey;
      luks_password  = lukspassword;
      systems        = { inherit base alt; };
    };

  grub =
    let
      hostname = "reboop-test-grub";
      base = mkSystem ./grub.nix hostname "base";
      alt  = mkSystem ./grub.nix hostname "alt";
      image = import ./image-grub.nix { inherit pkgs qemuBinary base alt; };
    in
    bundle "grub" {
      inherit hostname;
      disk_images    = [ "${image}/disk-1.qcow2" "${image}/disk-2.qcow2" ];
      ovmf           = null;
      initrd_key_pub = null;
      luks_password  = null;
      systems        = { inherit base alt; };
    };
}
