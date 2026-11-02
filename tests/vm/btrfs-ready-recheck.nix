# Works around a race in systemd's 64-btrfs.rules (as of systemd 262), for
# any NixOS machine.  The rules mark each member of a multi-device btrfs not
# ready until the kernel has seen them all, and the member that completes
# the filesystem then retriggers the members that udev's database lists as
# not ready.  A member whose worker hasn't written the database yet is
# missed and stays not ready, and as the member processed last, it can hold
# /dev/disk/by-label/<label> or by-uuid, so the initrd waits for the root
# until it gives up (see https://github.com/systemd/systemd/issues/41552).
#
# So once a member's not-ready state is in the database (udev runs RUN
# after writing it), check again, and retrigger the member if the filesystem
# has been completed meanwhile: either the completing member found this one
# in the database, or it completed the filesystem before this check.  A
# member of a filesystem that stays incomplete checks once per event and
# stays not ready.  Drop this once systemd's rules do the same.
{ config, lib, pkgs, ... }:

let
  btrfs = "${pkgs.btrfs-progs}/bin/btrfs";

  # The udev rule, for the udev whose udevadm is at `udevadm`.
  rule = udevadm: ''
    ACTION!="remove", ENV{ID_BTRFS_READY}=="0", RUN+="${pkgs.runtimeShell} -c 'if ${btrfs} device ready $devnode; then ${udevadm} trigger --action=change $devnode; fi'"
  '';
in
{
  config = lib.mkMerge [
    (lib.mkIf (config.boot.initrd.supportedFilesystems.btrfs or false) {
      boot.initrd.services.udev.rules = rule "${config.boot.initrd.systemd.package}/bin/udevadm";
      # Otherwise these would be in the initrd only as long as other
      # modules want them there.
      boot.initrd.systemd.storePaths = [ pkgs.runtimeShell btrfs ];
    })
    (lib.mkIf (config.boot.supportedFilesystems.btrfs or false) {
      services.udev.extraRules = rule "${config.systemd.package}/bin/udevadm";
    })
  ];
}
