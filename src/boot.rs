// Model-output: Claude Opus 5.5

//! What a machine's boot loader will boot next: systemd-boot's default
//! entry, or GRUB's in each /boot that GRUB is installed to (several with
//! NixOS's mirroredBoots), and what the UEFI firmware might do instead.

use crate::ssh::{QUICK, Session, shell_quote};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;

/// Where EFI variables are, and the vendor GUIDs of the ones we read.
const EFIVARS: &str = "/sys/firmware/efi/efivars";
const EFI_GLOBAL: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
const SYSTEMD_BOOT_VENDOR: &str = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

/// Filesystem types that a /boot might have, and that can be looked at
/// without the risk of hanging (unlike, say, NFS) or mounting something.
const LOCAL_FILESYSTEMS: &str = "ext2,ext3,ext4,vfat,btrfs,xfs,f2fs";

/// A boot loader's default entry: what the machine will boot next.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DefaultBoot {
    /// Which boot loader configuration it's from: "systemd-boot", or the
    /// grub.cfg, like "/boot-fallback/grub/grub.cfg".
    pub loader: String,
    /// systemd-boot's id for the entry, or GRUB's title.
    pub entry: String,
    /// The kernel command line.
    pub options: String,
    /// The files it needs: its kernel (or UKI) and initrds, or for GRUB,
    /// grub.cfg itself.
    pub files: Vec<String>,
    /// Those of `files` that aren't there.
    pub missing_files: Vec<String>,
}

impl DefaultBoot {
    /// Whether its configuration itself is missing, like a GRUB mirror's
    /// grub.cfg.
    pub fn is_missing(&self) -> bool {
        self.missing_files.contains(&self.loader)
    }

    /// The NixOS system it boots: the directory of its init= parameter (the
    /// last one, which is what counts), if that's a store path's init.
    pub fn system(&self) -> Option<&str> {
        let init = self.options.split_whitespace().rev().find_map(|option| option.strip_prefix("init="))?;
        init.strip_suffix("/init").filter(|system| system.starts_with("/nix/store/"))
    }

    /// The addresses the initrd will take statically: the client address of
    /// each ip= parameter that has one (not, e.g., "ip=dhcp").
    pub fn initrd_addresses(&self) -> Vec<Ipv4Addr> {
        let addresses = self.options.split_whitespace().filter_map(|option| option.strip_prefix("ip="));
        addresses.filter_map(|value| value.split(':').next()?.parse().ok()).collect()
    }
}

/// An entry that `bootctl list --json` shows.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootctlEntry {
    id: String,
    is_default: bool,
    /// Where the files' paths are relative to, e.g. "/boot"
    root: Option<String>,
    linux: Option<String>,
    /// A UKI or other EFI program, with its command line built in
    efi: Option<String>,
    initrd: Option<Vec<String>>,
    options: Option<String>,
    /// The whole command line, including a UKI's
    cmdline: Option<String>,
}

/// The default among the entries in `json`, from `bootctl list --json`, or
/// `None` if there's none.  Its missing_files are yet to be found.
fn parse_bootctl(json: &str) -> Result<Option<DefaultBoot>> {
    let entries: Vec<BootctlEntry> = serde_json::from_str(json).with_context(|| format!("unexpected bootctl output {json:?}"))?;
    let defaults: Vec<_> = entries.into_iter().filter(|entry| entry.is_default).collect();
    ensure!(defaults.len() <= 1, "bootctl shows several default entries: {defaults:?}");
    let Some(entry) = defaults.into_iter().next() else { return Ok(None) };
    let root = entry.root.unwrap_or_default();
    let files = entry.linux.iter().chain(&entry.efi).chain(entry.initrd.iter().flatten()).map(|file| format!("{root}{file}")).collect();
    Ok(Some(DefaultBoot {
        loader: "systemd-boot".into(),
        entry: entry.id,
        options: entry.options.or(entry.cmdline).unwrap_or_default(),
        files,
        missing_files: vec![],
    }))
}

/// A GRUB menu item: an entry to boot, or a submenu of more.
#[derive(Debug, PartialEq, Eq)]
enum MenuItem {
    /// The lines inside the entry, trimmed
    Entry { title: String, lines: Vec<String> },
    Submenu { title: String, items: Vec<MenuItem> },
}

impl MenuItem {
    fn title(&self) -> &str {
        match self {
            MenuItem::Entry { title, .. } | MenuItem::Submenu { title, .. } => title,
        }
    }
}

/// The title in a line like `menuentry "TITLE" --class nixos {`, which may
/// have backslash-escaped characters in quotes.
fn menu_title(line: &str) -> Result<String> {
    let rest = line.split_once(char::is_whitespace).map_or("", |(_, rest)| rest.trim_start());
    let mut chars = rest.chars();
    let Some(quote @ ('"' | '\'')) = chars.next() else {
        return rest.split_whitespace().next().map(str::to_string).ok_or_else(|| anyhow!("no title in {line:?}"));
    };
    let mut title = String::new();
    while let Some(c) = chars.next() {
        match c {
            '\\' if quote == '"' => title.extend(chars.next()),
            c if c == quote => return Ok(title),
            c => title.push(c),
        }
    }
    bail!("unterminated title in {line:?}")
}

/// The menu in grub.cfg's text `config`: its menuentry and submenu blocks,
/// each from a line like `menuentry "TITLE" ... {` to a line `}`.  Other
/// blocks, like functions, and comments are skipped.
fn parse_menu(config: &str) -> Result<Vec<MenuItem>> {
    // The blocks that are open, None for those that aren't menu items
    let mut open: Vec<Option<MenuItem>> = Vec::new();
    let mut menu = Vec::new();
    for line in config.lines().map(str::trim).filter(|line| !line.starts_with('#')) {
        if line.ends_with('{') {
            open.push(match line.split_whitespace().next() {
                Some("menuentry") => Some(MenuItem::Entry { title: menu_title(line)?, lines: vec![] }),
                Some("submenu") => Some(MenuItem::Submenu { title: menu_title(line)?, items: vec![] }),
                _ => None,
            });
        } else if line == "}" {
            let Some(item) = open.pop().ok_or_else(|| anyhow!("unbalanced braces in grub.cfg"))? else { continue };
            let submenu = open.iter_mut().rev().find_map(|block| match block {
                Some(MenuItem::Submenu { items, .. }) => Some(items),
                _ => None,
            });
            submenu.unwrap_or(&mut menu).push(item);
        } else if let Some(Some(MenuItem::Entry { lines, .. })) = open.last_mut() {
            lines.push(line.to_string());
        }
    }
    ensure!(open.is_empty(), "unbalanced braces in grub.cfg");
    Ok(menu)
}

/// The variables in a grubenv file.
fn parse_grubenv(grubenv: &str) -> BTreeMap<String, String> {
    grubenv
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// The default entry of the GRUB menu in `config` as GRUB will see it with
/// the variables in `env` (from grubenv): grub-reboot's next_entry, or else
/// the `set default=` that grub.cfg has for that case, which may name
/// saved_entry.  It's a path like "1>0" or "Submenu title>Entry title".
fn default_path(config: &str, env: &BTreeMap<String, String>) -> Result<String> {
    if let Some(next) = env.get("next_entry").filter(|next| !next.is_empty()) {
        return Ok(next.clone());
    }
    let unquote = |value: &str| value.trim_matches('"').to_string();
    let defaults: Vec<_> = config
        .lines()
        .filter_map(|line| line.trim().strip_prefix("set default="))
        .map(unquote)
        .filter(|value| value != "${next_entry}")
        .collect();
    let default = match &defaults[..] {
        [] => String::new(),
        [default] if default == "${saved_entry}" => env.get("saved_entry").cloned().unwrap_or_default(),
        [default] => default.clone(),
        several => bail!("grub.cfg sets its default several ways: {several:?}"),
    };
    Ok(default)
}

/// The entry that GRUB boots given the default `path` (see [`default_path`])
/// in `menu`: each step is an index or a title, and like GRUB, one that
/// leads nowhere (or is missing, when the path ends at a submenu) means that
/// menu's first item.
fn find_entry<'a>(menu: &'a [MenuItem], path: &str) -> Option<&'a MenuItem> {
    let mut items = menu;
    let mut steps = path.split('>');
    loop {
        let step = steps.next().unwrap_or("");
        let item = match step.parse::<usize>() {
            Ok(index) => items.get(index),
            Err(_) => items.iter().find(|item| item.title() == step),
        };
        match item.or(items.first())? {
            entry @ MenuItem::Entry { .. } => return Some(entry),
            MenuItem::Submenu { items: inside, .. } => items = inside,
        }
    }
}

/// Where the file that GRUB calls `path`, like "($drive1)//kernels/NAME",
/// is on the running machine, given the `boot_path` whose grub.cfg names it.
/// (A path's start depends on the filesystem GRUB reads it from, and any
/// btrfs subvolume, but its end is a store path or a /boot's kernels
/// directory.)
fn grub_file(boot_path: &str, path: &str) -> String {
    let path = path.strip_prefix('(').and_then(|rest| rest.split_once(')')).map_or(path, |(_, rest)| rest);
    let boot_path = boot_path.trim_end_matches('/');
    if let Some(start) = path.find("/nix/store/") {
        path[start..].to_string()
    } else if let Some(start) = path.rfind("/kernels/") {
        format!("{boot_path}{}", &path[start..])
    } else {
        format!("{boot_path}/{}", path.trim_start_matches('/'))
    }
}

/// The default in the grub.cfg of the /boot at `boot_path`, whose text is
/// `config` (`None` if it's missing), given its grubenv's text.  Its
/// missing_files are yet to be found.
fn parse_grub(boot_path: &str, config: Option<&str>, grubenv: &str) -> Result<DefaultBoot> {
    let loader = format!("{}/grub/grub.cfg", boot_path.trim_end_matches('/'));
    let Some(config) = config else {
        return Ok(DefaultBoot { entry: String::new(), options: String::new(), files: vec![loader.clone()], loader, missing_files: vec![] });
    };
    let context = || format!("in {loader}");
    let menu = parse_menu(config).with_context(context)?;
    let path = default_path(config, &parse_grubenv(grubenv)).with_context(context)?;
    let Some(MenuItem::Entry { title, lines }) = find_entry(&menu, &path) else {
        bail!("{loader} has no menu entries");
    };
    // The kernel and its options are on the `linux` line, or with Xen, on
    // the first `module` line after the hypervisor's `multiboot`.  The rest
    // are initrds.
    let mut options = String::new();
    let mut files = Vec::new();
    let mut kernel_found = false;
    for line in lines {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("linux" | "module") if !kernel_found => {
                kernel_found = true;
                files.extend(words.next().map(|kernel| grub_file(boot_path, kernel)));
                options = words.collect::<Vec<_>>().join(" ");
            }
            Some("initrd" | "module") => files.extend(words.map(|initrd| grub_file(boot_path, initrd))),
            Some("multiboot") => files.extend(words.next().map(|xen| grub_file(boot_path, xen))),
            _ => {}
        }
    }
    Ok(DefaultBoot { loader, entry: title.clone(), options, files, missing_files: vec![] })
}

/// Parses `findmnt --json --output TARGET` into mountpoints.
fn parse_mountpoints(json: &str) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Findmnt {
        filesystems: Vec<Mount>,
    }
    #[derive(Deserialize)]
    struct Mount {
        target: String,
    }
    let findmnt: Findmnt = serde_json::from_str(json).with_context(|| format!("unexpected findmnt output {json:?}"))?;
    Ok(findmnt.filesystems.into_iter().map(|mount| mount.target).collect())
}

/// Each /boot that GRUB is installed to (that has a grub directory), among
/// /boot itself and the mountpoints of local filesystems.
fn grub_boot_paths(session: &mut Session) -> Result<Vec<String>> {
    let command = format!("findmnt --list --json --output TARGET --types {LOCAL_FILESYSTEMS}");
    let output = session.run(&command, QUICK)?;
    // findmnt exits 1 without output when nothing matches.
    let mut candidates = if output.status == 1 && output.stdout.is_empty() { vec![] } else { parse_mountpoints(&output.stdout_text())? };
    candidates.push("/boot".into());
    candidates.sort();
    candidates.dedup();
    let words: Vec<_> = candidates.iter().map(|path| shell_quote(path)).collect();
    let script = format!(r#"for m in {}; do [ -d "${{m%/}}/grub" ] && printf '%s\n' "$m"; done; true"#, words.join(" "));
    Ok(session.run_ok(&script, QUICK)?.lines().map(str::to_string).collect())
}

/// The contents of the EFI variable `name` from `vendor`, without its
/// attributes, or `None` if it isn't set (or there's no UEFI).
fn efi_variable(session: &mut Session, name: &str, vendor: &str) -> Result<Option<Vec<u8>>> {
    let output = session.run(&format!("cat -- {}", shell_quote(&format!("{EFIVARS}/{name}-{vendor}"))), QUICK)?;
    Ok((output.status == 0 && output.stdout.len() >= 4).then(|| output.stdout[4..].to_vec()))
}

/// Whether systemd-boot booted the machine, going by its LoaderInfo EFI
/// variable, whose value is `loader_info` (UTF-16, like "systemd-boot 260").
/// GRUB's bli module sets it too.
fn is_systemd_boot(loader_info: &[u8]) -> bool {
    let units: Vec<u16> = loader_info.as_chunks::<2>().0.iter().map(|&pair| u16::from_le_bytes(pair)).collect();
    String::from_utf16_lossy(&units).starts_with("systemd-boot")
}

/// What the machine at the other end of `session` will boot next:
/// systemd-boot's default entry if systemd-boot booted it, or else GRUB's
/// default in each /boot that GRUB is installed to.  Empty if there's
/// neither.
pub fn default_boots(session: &mut Session) -> Result<Vec<DefaultBoot>> {
    let mut boots = Vec::new();
    if efi_variable(session, "LoaderInfo", SYSTEMD_BOOT_VENDOR)?.is_some_and(|info| is_systemd_boot(&info)) {
        let json = session.run_ok("bootctl list --json=short --no-pager", QUICK)?;
        boots.extend(parse_bootctl(&json)?);
    } else {
        for boot_path in grub_boot_paths(session)? {
            let grub = format!("{}/grub", boot_path.trim_end_matches('/'));
            let config = session.run(&format!("cat -- {}", shell_quote(&format!("{grub}/grub.cfg"))), QUICK)?;
            let config = (config.status == 0).then(|| config.stdout_text());
            let grubenv = session.run(&format!("cat -- {}", shell_quote(&format!("{grub}/grubenv"))), QUICK)?.stdout_text();
            boots.push(parse_grub(&boot_path, config.as_deref(), &grubenv)?);
        }
    }

    let files: Vec<_> = boots.iter().flat_map(|boot| &boot.files).map(|file| shell_quote(file)).collect();
    if !files.is_empty() {
        let script = format!(r#"for f in {}; do [ -f "$f" ] || printf '%s\n' "$f"; done"#, files.join(" "));
        let missing: Vec<String> = session.run_ok(&script, QUICK)?.lines().map(str::to_string).collect();
        for boot in &mut boots {
            boot.missing_files = boot.files.iter().filter(|file| missing.contains(file)).cloned().collect();
        }
    }
    Ok(boots)
}

/// What the UEFI firmware is set to do at the next boot instead of starting
/// the usual boot loader, for people, given the values of its BootNext and
/// OsIndications variables (if set).
fn parse_firmware_overrides(boot_next: Option<&[u8]>, os_indications: Option<&[u8]>) -> Vec<String> {
    let mut overrides = Vec::new();
    if let Some(&[low, high]) = boot_next {
        let option = u16::from_le_bytes([low, high]);
        overrides.push(format!("the firmware will boot Boot{option:04X} next (BootNext), whatever the boot loader's default"));
    }
    // EFI_OS_INDICATIONS_BOOT_TO_FW_UI
    if os_indications.and_then(|bytes| bytes.first()).is_some_and(|&low| low & 1 != 0) {
        overrides.push("the firmware will open its setup at the next boot (OsIndications)".into());
    }
    overrides
}

/// What the machine's UEFI firmware is set to do at the next boot instead of
/// starting the usual boot loader: nothing, usually.
pub fn firmware_overrides(session: &mut Session) -> Result<Vec<String>> {
    let boot_next = efi_variable(session, "BootNext", EFI_GLOBAL)?;
    let os_indications = efi_variable(session, "OsIndications", EFI_GLOBAL)?;
    Ok(parse_firmware_overrides(boot_next.as_deref(), os_indications.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYSTEM: &str = "/nix/store/58irgx61icng5h3yhs4nca3z2wndq3wd-nixos-system-reboop-test-grub-26.05pre-git";
    const OLD_SYSTEM: &str = "/nix/store/liczkacy2jnlcdrmln8dk8ygzb1afil9-nixos-system-reboop-test-grub-26.05pre-git";

    /// A menu entry as NixOS writes it, booting `system` with kernels copied
    /// to /boot.
    fn grub_entry(title: &str, system: &str) -> String {
        format!(
            "menuentry \"{title}\" --class nixos {{\n\
             search --set=drive1 --fs-uuid 898110b7\n  \
             linux ($drive1)//kernels/ls9x-linux-6.18.54-bzImage init={system}/init console=ttyS0 ip=192.168.10.12::192.168.10.1:255.255.255.0:one::none\n  \
             initrd ($drive1)//kernels/b09i-initrd-linux-6.18.54-initrd\n\
             }}\n"
        )
    }

    /// A grub.cfg like NixOS's, with `default` in the case without
    /// grub-reboot, and a rollback: the default entry boots an older system
    /// than the newest in the submenu.
    fn grub_cfg(default: &str) -> String {
        format!(
            "# Automatically generated.  DO NOT EDIT THIS FILE!\n    \
             if [ \"${{next_entry}}\" ]; then\n      set default=\"${{next_entry}}\"\n      set next_entry=\n    \
             else\n      set default={default}\n      set timeout=0\n    fi\n    \
             function savedefault {{\n        saved_entry=\"${{chosen}}\"\n    }}\n\
             # a comment that ends like a block {{\n\
             {}submenu \"NixOS - All configurations\" --class submenu {{\n{}{}}}\n",
            grub_entry("NixOS", SYSTEM),
            grub_entry("NixOS - Configuration 3 (2026-09-28)", OLD_SYSTEM),
            grub_entry("NixOS - Configuration 2 (2026-09-27)", SYSTEM),
        )
    }

    #[test]
    fn parses_grub_menus() {
        let menu = parse_menu(&grub_cfg("0")).unwrap();
        let titles: Vec<_> = menu.iter().map(MenuItem::title).collect();
        assert_eq!(titles, ["NixOS", "NixOS - All configurations"]);
        let MenuItem::Submenu { items, .. } = &menu[1] else { panic!("{menu:?}") };
        assert_eq!(items.len(), 2);
        assert!(parse_menu("menuentry \"x\" {\n").is_err());
        assert!(parse_menu("}\n").is_err());
        assert_eq!(menu_title("submenu 'a b' {").unwrap(), "a b");
        assert_eq!(menu_title(r#"menuentry "NixOS - Configuration 5 (foo \"bar\")" {"#).unwrap(), r#"NixOS - Configuration 5 (foo "bar")"#);
        assert!(menu_title(r#"menuentry "unterminated {"#).is_err());
    }

    #[test]
    fn finds_grubs_default() {
        let boot = parse_grub("/boot-fallback", Some(&grub_cfg("0")), "# GRUB Environment Block\n####").unwrap();
        assert_eq!((boot.loader.as_str(), boot.entry.as_str(), boot.system()), ("/boot-fallback/grub/grub.cfg", "NixOS", Some(SYSTEM)));
        assert_eq!(boot.files, ["/boot-fallback/kernels/ls9x-linux-6.18.54-bzImage", "/boot-fallback/kernels/b09i-initrd-linux-6.18.54-initrd"]);
        assert_eq!(boot.initrd_addresses(), [Ipv4Addr::new(192, 168, 10, 12)]);

        let entry = |default: &str, grubenv: &str| {
            let boot = parse_grub("/boot", Some(&grub_cfg(default)), grubenv).unwrap();
            (boot.entry.clone(), boot.system().unwrap().to_string())
        };
        let newest = ("NixOS - Configuration 3 (2026-09-28)".to_string(), OLD_SYSTEM.to_string());
        let older = ("NixOS - Configuration 2 (2026-09-27)".to_string(), SYSTEM.to_string());
        // grub-reboot, by index or by title
        assert_eq!(entry("0", "next_entry=1>0\n"), newest);
        assert_eq!(entry("0", "next_entry=NixOS - All configurations>NixOS - Configuration 2 (2026-09-27)\n"), older);
        // boot.loader.grub.default = "saved"
        assert_eq!(entry("\"${saved_entry}\"", "saved_entry=1>1\n"), older);
        assert_eq!(entry("\"${saved_entry}\"", "").0, "NixOS");
        // Like GRUB, a menu's first item when a step leads nowhere, or when
        // the path ends at a submenu
        assert_eq!(entry("7", "").0, "NixOS");
        assert_eq!(entry("1>99", ""), newest);
        assert_eq!(entry("1", ""), newest);
        assert_eq!(entry("0", "next_entry=NixOS - All configurations\n"), newest);
    }

    #[test]
    fn reads_grub_entries_and_mirrors() {
        let xen = "menuentry \"NixOS\" {\n  multiboot ($drive1)//kernels/xen.gz dom0_mem=4G\n  \
                   module ($drive1)//kernels/bzImage init=/nix/store/abc-nixos-system/init quiet\n  module ($drive1)//kernels/initrd\n}\n";
        let boot = parse_grub("/boot", Some(xen), "").unwrap();
        assert_eq!(boot.system(), Some("/nix/store/abc-nixos-system"));
        assert_eq!(boot.files, ["/boot/kernels/xen.gz", "/boot/kernels/bzImage", "/boot/kernels/initrd"]);

        // A mirror whose grub.cfg is gone
        let boot = parse_grub("/boot-fallback/", None, "").unwrap();
        assert_eq!(boot.files, ["/boot-fallback/grub/grub.cfg"]);
        let boot = DefaultBoot { missing_files: boot.files.clone(), ..boot };
        assert!(boot.is_missing());
    }

    #[test]
    fn maps_grubs_files() {
        assert_eq!(grub_file("/boot", "($drive1)//kernels/k"), "/boot/kernels/k");
        assert_eq!(grub_file("/boot/", "($drive2)/@root/boot/kernels/k"), "/boot/kernels/k");
        assert_eq!(grub_file("/boot", "($drive2)/@/nix/store/abc-linux/bzImage"), "/nix/store/abc-linux/bzImage");
        assert_eq!(grub_file("/boot", "/vmlinuz"), "/boot/vmlinuz");
    }

    #[test]
    fn finds_systemd_boots_default() {
        let json = r#"[{"type":"type1","id":"nixos-generation-24.conf","root":"/boot","options":"init=/nix/store/hcib-nixos-system-one/init console=ttyS0","linux":"/EFI/nixos/ls9x-bzImage.efi","initrd":["/EFI/nixos/y89c-initrd.efi"],"isDefault":true},
                       {"type":"type1","id":"nixos-generation-23.conf","root":"/boot","options":"init=/nix/store/jqki-nixos-system-one/init","linux":"/EFI/nixos/ls9x-bzImage.efi","initrd":null,"isDefault":false},
                       {"type":"auto","id":"auto-reboot-to-firmware-setup","isDefault":false}]"#;
        let boot = parse_bootctl(json).unwrap().unwrap();
        assert_eq!((boot.entry.as_str(), boot.system()), ("nixos-generation-24.conf", Some("/nix/store/hcib-nixos-system-one")));
        assert_eq!(boot.files, ["/boot/EFI/nixos/ls9x-bzImage.efi", "/boot/EFI/nixos/y89c-initrd.efi"]);
        assert!(boot.initrd_addresses().is_empty());
        assert!(parse_bootctl("[]").unwrap().is_none());
        assert!(parse_bootctl(&json.replace("false", "true")).is_err());

        // A UKI, with its command line built in
        let uki = r#"[{"type":"type2","id":"nixos-30.efi","root":"/boot","efi":"/EFI/Linux/nixos-30.efi","cmdline":"init=/nix/store/abc-nixos-system/init","isDefault":true}]"#;
        let boot = parse_bootctl(uki).unwrap().unwrap();
        assert_eq!((boot.system(), &boot.files[..]), (Some("/nix/store/abc-nixos-system"), &["/boot/EFI/Linux/nixos-30.efi".to_string()][..]));
    }

    #[test]
    fn reads_kernel_options() {
        let boot = |options: &str| DefaultBoot {
            loader: "systemd-boot".into(),
            entry: "e".into(),
            options: options.into(),
            files: vec![],
            missing_files: vec![],
        };
        assert_eq!(boot("init=/nix/store/a-nixos-system/init").system(), Some("/nix/store/a-nixos-system"));
        // The last init= counts.
        assert_eq!(boot("init=/nix/store/a-nixos-system/init init=/bin/sh").system(), None);
        assert_eq!(boot("init=/bin/sh init=/nix/store/a-nixos-system/init").system(), Some("/nix/store/a-nixos-system"));
        assert_eq!(boot("init=/sbin/init").system(), None);
        assert_eq!(boot("quiet").system(), None);
        let addresses = boot("ip=dhcp ip=:::::eth0:dhcp ip=10.0.0.5::10.0.0.1:255.255.255.0::eth1:none ip=[fd00::5]::[fd00::1]:64::eth2:none").initrd_addresses();
        assert_eq!(addresses, [Ipv4Addr::new(10, 0, 0, 5)]);
    }

    #[test]
    fn reads_efi_variables() {
        let utf16 = |text: &str| text.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>();
        assert!(is_systemd_boot(&utf16("systemd-boot 260.4\0")));
        assert!(!is_systemd_boot(&utf16("GRUB 2.12\0")));
        assert_eq!(parse_firmware_overrides(None, Some(&[0, 0, 0, 0, 0, 0, 0, 0])), Vec::<String>::new());
        assert_eq!(
            parse_firmware_overrides(Some(&[0x03, 0x00]), Some(&[0x01, 0, 0, 0, 0, 0, 0, 0])),
            [
                "the firmware will boot Boot0003 next (BootNext), whatever the boot loader's default",
                "the firmware will open its setup at the next boot (OsIndications)",
            ]
        );
    }

    #[test]
    fn parses_mountpoints() {
        let json = r#"{"filesystems": [{"target": "/boot"}, {"target": "/boot mirror"}]}"#;
        assert_eq!(parse_mountpoints(json).unwrap(), ["/boot", "/boot mirror"]);
    }
}
