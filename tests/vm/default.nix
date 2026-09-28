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
  vmTools = pkgs.vmTools.override { customQemu = qemuCommon.qemuBinary pkgs.qemu_test; };

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

  # Shell commands for an image builder (see image-*.nix) that install `base`
  # as the system profile, with its boot loader, into the filesystems mounted
  # at /mnt, and copy `alt` into the store for tests to switch to.
  install = base: alt:
    let
      closure = pkgs.closureInfo { rootPaths = [ base alt ]; };
    in
    ''
      export HOME=$TMPDIR
      export NIX_STATE_DIR=$TMPDIR/state
      nix-store --load-db < ${closure}/registration

      nixos-install --root /mnt --no-bootloader --no-root-passwd --no-channel-copy \
        --system ${base} --substituters ""
      nix --extra-experimental-features nix-command copy --no-check-sigs --to /mnt ${alt}
      ln -s ${alt} /mnt/nix/var/nix/gcroots/reboop-test-alt

      # Without the build's NIX_STATE_DIR and HOME, which would leave files in /tmp.
      env -u NIX_STATE_DIR HOME=/root NIXOS_INSTALL_BOOTLOADER=1 nixos-enter --root /mnt -- \
        /nix/var/nix/profiles/system/bin/switch-to-configuration boot
    '';

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
      image = import ./image-systemd-boot.nix { inherit pkgs vmTools lukspassword; install = install base alt; };
    in
    bundle "systemd-boot" {
      inherit hostname;
      disk_images = [ "${image}/disk.qcow2" ];
      ovmf        = { code = "${pkgs.OVMF.fd}/FV/OVMF_CODE.fd"; vars = "${pkgs.OVMF.fd}/FV/OVMF_VARS.fd"; };
      initrd      = { host_key_pub = sshKeys.snakeOilPublicKey; luks_password = lukspassword; };
      systems     = { inherit base alt; };
    };

  grub =
    let
      hostname = "reboop-test-grub";
      base = mkSystem ./grub.nix hostname "base";
      alt  = mkSystem ./grub.nix hostname "alt";
      image = import ./image-grub.nix { inherit pkgs vmTools; install = install base alt; };
    in
    bundle "grub" {
      inherit hostname;
      disk_images = [ "${image}/disk-1.qcow2" "${image}/disk-2.qcow2" ];
      ovmf        = null;
      initrd      = null;
      systems     = { inherit base alt; };
    };
}
