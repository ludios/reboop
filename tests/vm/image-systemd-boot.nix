# Model-output: Claude Opus 5.5
#
# A qcow2 disk image for systemd-boot.nix with a GPT, an ESP with
# systemd-boot, and a LUKS2 partition holding a btrfs root.  `install` (from
# default.nix) installs the systems onto it.
{ pkgs, vmTools, install, lukspassword }:

vmTools.runInLinuxVM (
  pkgs.runCommand "reboop-test-image"
    {
      memSize = 2048;
      nativeBuildInputs = with pkgs; [
        btrfs-progs
        cryptsetup
        dosfstools
        nix
        nixos-install-tools
        util-linux
      ];
      preVM = ''
        mkdir $out
        diskImage=$(pwd)/disk.raw
        truncate -s 8G $diskImage
      '';
      postVM = ''
        ${pkgs.qemu_test}/bin/qemu-img convert -f raw -O qcow2 $diskImage $out/disk.qcow2
      '';
    }
    ''
      # There's no udev here, so have libdevmapper create device nodes itself.
      export DM_DISABLE_UDEV=1

      sfdisk --wipe always /dev/vda <<EOF
      label: gpt
      name=ESP, size=256MiB, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B
      name=root, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4
      EOF
      partx -u /dev/vda || true

      mkfs.vfat -F 32 -n ESP /dev/vda1

      # A cheap KDF so that unlocking doesn't slow down every boot.
      printf %s ${pkgs.lib.escapeShellArg lukspassword} |
        cryptsetup luksFormat --batch-mode --type luks2 \
          --pbkdf pbkdf2 --pbkdf-force-iterations 1000 --key-file - /dev/vda2
      printf %s ${pkgs.lib.escapeShellArg lukspassword} |
        cryptsetup open --key-file - /dev/vda2 root

      mkfs.btrfs -L root /dev/mapper/root
      mkdir /mnt
      mount -o compress=zstd /dev/mapper/root /mnt
      mkdir /mnt/boot
      mount /dev/vda1 /mnt/boot

      # bootctl finds the ESP's partition through /dev/block/MAJOR:MINOR.
      mkdir -p /dev/block
      ln -s /dev/vda1 /dev/block/$(cat /sys/class/block/vda1/dev)

      ${install}

      umount -R /mnt
      cryptsetup close root
    ''
)
