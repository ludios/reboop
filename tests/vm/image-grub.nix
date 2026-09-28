# Model-output: Claude Opus 5.5
#
# Two qcow2 disk images for grub.nix, each with a GPT, a BIOS boot partition
# for GRUB, an ext4 /boot of its own (boot-1 for /boot, boot-2 for
# /boot-fallback), and half of a btrfs RAID1 root.  `install` (from
# default.nix) installs the systems onto them.
{ pkgs, vmTools, install }:

vmTools.runInLinuxVM (
  pkgs.runCommand "reboop-test-grub-image"
    {
      memSize = 2048;
      nativeBuildInputs = with pkgs; [
        btrfs-progs
        e2fsprogs
        nix
        nixos-install-tools
        util-linux
      ];
      preVM = ''
        mkdir $out
        diskImage=$(pwd)/disk-1.raw
        diskImage2=$(pwd)/disk-2.raw
        truncate -s 8G $diskImage $diskImage2
        QEMU_OPTS+=" -drive file=$diskImage2,if=virtio,cache=unsafe,werror=report"
      '';
      postVM = ''
        ${pkgs.qemu_test}/bin/qemu-img convert -f raw -O qcow2 $diskImage $out/disk-1.qcow2
        ${pkgs.qemu_test}/bin/qemu-img convert -f raw -O qcow2 $diskImage2 $out/disk-2.qcow2
      '';
    }
    ''
      i=0
      for disk in /dev/vda /dev/vdb; do
        i=$((i + 1))
        printf '%s\n' "label: gpt" \
          "name=bios, size=1MiB, type=21686148-6449-6E6F-744E-656564454649" \
          "name=boot-$i, size=256MiB, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4" \
          "name=root-$i, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4" |
          sfdisk --wipe always $disk
        partx -u $disk || true
        mkfs.ext4 -q ''${disk}2
      done

      mkfs.btrfs -q -L root -d raid1 -m raid1 /dev/vda3 /dev/vdb3
      # There's no udev (or /dev/btrfs-control) here to tell the kernel
      # about both devices, so name them.
      mkdir /mnt
      mount -o compress=zstd,device=/dev/vda3,device=/dev/vdb3 /dev/vda3 /mnt
      mkdir /mnt/boot /mnt/boot-fallback
      mount /dev/vda2 /mnt/boot
      mount /dev/vdb2 /mnt/boot-fallback

      ${install}

      umount -R /mnt
    ''
)
