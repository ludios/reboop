# Model-output: Claude Opus 5.5
#
# Two qcow2 disk images for grub.nix, each with a GPT, a BIOS boot partition
# for GRUB, an ext4 /boot of its own (boot-1 for /boot, boot-2 for
# /boot-fallback), and half of a btrfs RAID1 root.  `base` is installed as
# the system profile (and so is GRUB's default entry); `alt` is only copied
# into the store, for tests to switch to.
{ pkgs, qemuBinary, base, alt }:

let
  closure = pkgs.closureInfo { rootPaths = [ base alt ]; };
  vmTools = pkgs.vmTools.override { customQemu = qemuBinary; };
in
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
        truncate -s 8G $diskImage disk-2.raw
        QEMU_OPTS+=" -drive file=$(pwd)/disk-2.raw,if=virtio,cache=unsafe,werror=report"
      '';
      postVM = ''
        for i in 1 2; do
          ${pkgs.qemu_test}/bin/qemu-img convert -f raw -O qcow2 disk-$i.raw $out/disk-$i.qcow2
        done
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

      umount -R /mnt
    ''
)
