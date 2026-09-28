# Model-output: Claude Opus 5.5
#
# Everything the VM test suite needs, gathered into one directory:
#
#     nix-build tests/vm
#
# See manifest.json in the output for what's where.
{ nixpkgs ? <nixpkgs> }:

let
  pkgs = import nixpkgs { };
  qemuCommon = import "${nixpkgs}/nixos/lib/qemu-common.nix" { inherit (pkgs) lib stdenv; };

  # Test-only keys from RFC 9500, shipped with nixpkgs for use in VM tests.
  sshKeys = import "${nixpkgs}/nixos/tests/ssh-keys.nix" pkgs;

  lukspassword = "reboop test passphrase";

  mkSystem = variant:
    (import "${nixpkgs}/nixos/lib/eval-config.nix" {
      system = pkgs.stdenv.hostPlatform.system;
      modules = [
        ./configuration.nix
        { _module.args = { inherit variant sshKeys; }; }
      ];
    }).config.system.build.toplevel;

  base = mkSystem "base";
  alt  = mkSystem "alt";

  image = import ./image.nix {
    inherit pkgs base alt lukspassword;
    qemuBinary = qemuCommon.qemuBinary pkgs.qemu_test;
  };

  manifest = pkgs.writeText "manifest.json" (builtins.toJSON {
    disk_image     = "${image}/disk.qcow2";
    ovmf_code      = "${pkgs.OVMF.fd}/FV/OVMF_CODE.fd";
    ovmf_vars      = "${pkgs.OVMF.fd}/FV/OVMF_VARS.fd";
    qemu           = "${pkgs.qemu_test}/bin/qemu-system-x86_64";
    client_key     = "${sshKeys.snakeOilEd25519PrivateKey}";
    host_key_pub   = sshKeys.snakeOilEd25519PublicKey;
    initrd_key_pub = sshKeys.snakeOilPublicKey;
    luks_password  = lukspassword;
    systems        = { inherit base alt; };
  });
in
pkgs.runCommand "reboop-test-vm" { } ''
  mkdir $out
  ln -s ${manifest} $out/manifest.json
''
