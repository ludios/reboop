// Model-output: Claude Opus 5.5

//! What a machine's boot loader will boot next: systemd-boot's default
//! entry, or GRUB's in each /boot that GRUB is installed to (several with
//! NixOS's mirroredBoots).

use crate::ssh::{QUICK, Session, shell_quote};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;

/// The EFI variable in which systemd-boot says it booted the machine.
const SYSTEMD_BOOT_LOADER_INFO: &str = "/sys/firmware/efi/efivars/LoaderInfo-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

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
    /// Files the entry needs that aren't there: its kernel and initrds.
    pub missing_files: Vec<String>,
}

impl DefaultBoot {
    /// The NixOS system it boots: the directory of its init= parameter.
    pub fn system(&self) -> Option<&str> {
        self.options.split_whitespace().find_map(|option| option.strip_prefix("init=")?.strip_suffix("/init"))
    }

    /// The addresses the initrd will take statically: the client address of
    /// each ip= parameter that has one (not, e.g., "ip=dhcp").
    pub fn initrd_addresses(&self) -> Vec<Ipv4Addr> {
        let addresses = self.options.split_whitespace().filter_map(|option| option.strip_prefix("ip="));
        addresses.filter_map(|value| value.split(':').next()?.parse().ok()).collect()
    }
}

/// Which of `files` on the machine at the other end of `session` aren't
/// there.
fn missing_files(session: &mut Session, files: &[String]) -> Result<Vec<String>> {
    if files.is_empty() {
        return Ok(vec![]);
    }
    let words: Vec<_> = files.iter().map(|file| shell_quote(file)).collect();
    let script = format!(r#"for f in {}; do [ -f "$f" ] || printf '%s\n' "$f"; done"#, words.join(" "));
    Ok(session.run_ok(&script, QUICK)?.lines().map(str::to_string).collect())
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
    initrd: Option<Vec<String>>,
    options: Option<String>,
}

/// The default among the entries in `json`, from `bootctl list --json`, and
/// the files it needs; `None` if there's no default.
fn parse_bootctl(json: &str) -> Result<Option<(DefaultBoot, Vec<String>)>> {
    let entries: Vec<BootctlEntry> = serde_json::from_str(json).with_context(|| format!("unexpected bootctl output {json:?}"))?;
    let defaults: Vec<_> = entries.into_iter().filter(|entry| entry.is_default).collect();
    ensure!(defaults.len() <= 1, "bootctl shows several default entries: {defaults:?}");
    let Some(entry) = defaults.into_iter().next() else { return Ok(None) };
    let root = entry.root.unwrap_or_default();
    let files = entry.linux.iter().chain(entry.initrd.iter().flatten()).map(|file| format!("{root}{file}")).collect();
    let boot = DefaultBoot {
        loader: "systemd-boot".into(),
        entry: entry.id,
        options: entry.options.unwrap_or_default(),
        missing_files: vec![],
    };
    Ok(Some((boot, files)))
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

/// The title in a line like `menuentry "TITLE" --class nixos {`.
fn menu_title(line: &str) -> Result<String> {
    let rest = line.split_once(char::is_whitespace).map_or("", |(_, rest)| rest.trim_start());
    let title = match rest.chars().next() {
        Some(quote @ ('"' | '\'')) => rest[1..].split(quote).next(),
        _ => rest.split_whitespace().next(),
    };
    title.map(str::to_string).ok_or_else(|| anyhow!("no title in {line:?}"))
}

/// The menu in grub.cfg's text `config`: its menuentry and submenu blocks,
/// each from a line like `menuentry "TITLE" ... {` to a line `}`.  Other
/// blocks, like functions, are skipped.
fn parse_menu(config: &str) -> Result<Vec<MenuItem>> {
    enum Block {
        Entry { title: String, lines: Vec<String> },
        Submenu { title: String, items: Vec<MenuItem> },
        Other,
    }
    let mut open: Vec<Block> = Vec::new();
    let mut menu = Vec::new();
    for line in config.lines().map(str::trim) {
        if line.ends_with('{') {
            open.push(match line.split_whitespace().next() {
                Some("menuentry") => Block::Entry { title: menu_title(line)?, lines: vec![] },
                Some("submenu") => Block::Submenu { title: menu_title(line)?, items: vec![] },
                _ => Block::Other,
            });
        } else if line == "}" {
            let item = match open.pop().ok_or_else(|| anyhow!("unbalanced braces in grub.cfg"))? {
                Block::Entry { title, lines } => MenuItem::Entry { title, lines },
                Block::Submenu { title, items } => MenuItem::Submenu { title, items },
                Block::Other => continue,
            };
            let submenu = open.iter_mut().rev().find_map(|block| match block {
                Block::Submenu { items, .. } => Some(items),
                _ => None,
            });
            submenu.unwrap_or(&mut menu).push(item);
        } else if let Some(Block::Entry { lines, .. }) = open.last_mut() {
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

/// The entry at `path` (see [`default_path`]) in `menu`: each step through
/// submenus is an index or a title.  Like GRUB, falls back to the first
/// item when the path leads nowhere.
fn find_entry<'a>(menu: &'a [MenuItem], path: &str) -> Option<&'a MenuItem> {
    let mut items = menu;
    let mut found = None;
    for step in path.split('>') {
        let item = match step.parse::<usize>() {
            Ok(index) => items.get(index),
            Err(_) => items.iter().find(|item| item.title() == step),
        };
        found = item;
        match item {
            Some(MenuItem::Submenu { items: inside, .. }) => items = inside,
            _ => break,
        }
    }
    match found {
        Some(entry @ MenuItem::Entry { .. }) => Some(entry),
        _ => menu.first(),
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
/// `config`, given its grubenv's text, and the files it needs.
fn parse_grub(boot_path: &str, config: &str, grubenv: &str) -> Result<(DefaultBoot, Vec<String>)> {
    let loader = format!("{}/grub/grub.cfg", boot_path.trim_end_matches('/'));
    let context = || format!("in {loader}");
    let menu = parse_menu(config).with_context(context)?;
    let path = default_path(config, &parse_grubenv(grubenv)).with_context(context)?;
    let Some(MenuItem::Entry { title, lines }) = find_entry(&menu, &path) else {
        bail!("{loader} has no menu entries");
    };
    let mut options = String::new();
    let mut files = Vec::new();
    for line in lines {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("linux") => {
                files.extend(words.next().map(|kernel| grub_file(boot_path, kernel)));
                options = words.collect::<Vec<_>>().join(" ");
            }
            Some("initrd") => files.extend(words.map(|initrd| grub_file(boot_path, initrd))),
            _ => {}
        }
    }
    Ok((DefaultBoot { loader, entry: title.clone(), options, missing_files: vec![] }, files))
}

/// What the machine at the other end of `session` will boot next:
/// systemd-boot's default entry if systemd-boot booted it, or else GRUB's
/// default in each /boot (any mountpoint, or /boot itself) with a
/// grub/grub.cfg.  Empty if there's neither.
pub fn default_boots(session: &mut Session) -> Result<Vec<DefaultBoot>> {
    let mut boots = Vec::new();
    if session.run(&format!("test -e {}", shell_quote(SYSTEMD_BOOT_LOADER_INFO)), QUICK)?.status == 0 {
        let json = session.run_ok("bootctl list --json=short --no-pager", QUICK)?;
        boots.extend(parse_bootctl(&json)?);
    } else {
        let find = r#"{ echo /boot; findmnt -rno TARGET; } | sort -u | while read -r m; do
            [ -f "${m%/}/grub/grub.cfg" ] && printf '%s\n' "$m"
        done; true"#;
        for boot_path in session.run_ok(find, QUICK)?.lines() {
            let grub = format!("{}/grub", boot_path.trim_end_matches('/'));
            let config = session.run_ok(&format!("cat -- {}", shell_quote(&format!("{grub}/grub.cfg"))), QUICK)?;
            let grubenv = session.run(&format!("cat -- {}", shell_quote(&format!("{grub}/grubenv"))), QUICK)?.stdout_text();
            boots.push(parse_grub(boot_path, &config, &grubenv)?);
        }
    }
    boots
        .into_iter()
        .map(|(mut boot, files)| {
            boot.missing_files = missing_files(session, &files)?;
            Ok(boot)
        })
        .collect()
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
    /// grub-reboot.
    fn grub_cfg(default: &str) -> String {
        format!(
            "# Automatically generated.  DO NOT EDIT THIS FILE!\n    \
             if [ \"${{next_entry}}\" ]; then\n      set default=\"${{next_entry}}\"\n      set next_entry=\n    \
             else\n      set default={default}\n      set timeout=0\n    fi\n    \
             function savedefault {{\n        saved_entry=\"${{chosen}}\"\n    }}\n\
             {}submenu \"NixOS - All configurations\" --class submenu {{\n{}{}}}\n",
            grub_entry("NixOS", SYSTEM),
            grub_entry("NixOS - Configuration 3 (2026-09-28)", SYSTEM),
            grub_entry("NixOS - Configuration 2 (2026-09-27)", OLD_SYSTEM),
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
    }

    #[test]
    fn finds_grubs_default() {
        let (boot, files) = parse_grub("/boot-fallback", &grub_cfg("0"), "# GRUB Environment Block\n####").unwrap();
        assert_eq!((boot.loader.as_str(), boot.entry.as_str(), boot.system()), ("/boot-fallback/grub/grub.cfg", "NixOS", Some(SYSTEM)));
        assert_eq!(files, ["/boot-fallback/kernels/ls9x-linux-6.18.54-bzImage", "/boot-fallback/kernels/b09i-initrd-linux-6.18.54-initrd"]);
        assert_eq!(boot.initrd_addresses(), [Ipv4Addr::new(192, 168, 10, 12)]);

        let system = |config: &str, grubenv: &str| {
            let (boot, _) = parse_grub("/boot", config, grubenv).unwrap();
            (boot.entry.clone(), boot.system().unwrap().to_string())
        };
        let old = ("NixOS - Configuration 2 (2026-09-27)".to_string(), OLD_SYSTEM.to_string());
        // grub-reboot, by index or by title
        assert_eq!(system(&grub_cfg("0"), "next_entry=1>1\n"), old);
        assert_eq!(system(&grub_cfg("0"), "next_entry=NixOS - All configurations>NixOS - Configuration 2 (2026-09-27)\n"), old);
        // boot.loader.grub.default = "saved"
        assert_eq!(system(&grub_cfg("\"${saved_entry}\""), "saved_entry=1>1\n"), old);
        assert_eq!(system(&grub_cfg("\"${saved_entry}\""), "").1, SYSTEM);
        // Like GRUB, the first entry when the default leads nowhere
        assert_eq!(system(&grub_cfg("7"), "").1, SYSTEM);
        assert_eq!(system(&grub_cfg("1"), "").1, SYSTEM);
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
        let (boot, files) = parse_bootctl(json).unwrap().unwrap();
        assert_eq!((boot.entry.as_str(), boot.system()), ("nixos-generation-24.conf", Some("/nix/store/hcib-nixos-system-one")));
        assert_eq!(files, ["/boot/EFI/nixos/ls9x-bzImage.efi", "/boot/EFI/nixos/y89c-initrd.efi"]);
        assert!(boot.initrd_addresses().is_empty());
        assert!(parse_bootctl("[]").unwrap().is_none());
        assert!(parse_bootctl(&json.replace("false", "true")).is_err());
    }

    #[test]
    fn reads_kernel_options() {
        let boot = |options: &str| DefaultBoot { loader: "systemd-boot".into(), entry: "e".into(), options: options.into(), missing_files: vec![] };
        assert_eq!(boot("init=/nix/store/a-nixos-system/init").system(), Some("/nix/store/a-nixos-system"));
        assert_eq!(boot("init=/sbin/init2 quiet").system(), None);
        assert_eq!(boot("quiet").system(), None);
        let addresses = boot("ip=dhcp ip=:::::eth0:dhcp ip=10.0.0.5::10.0.0.1:255.255.255.0::eth1:none ip=[fd00::5]::[fd00::1]:64::eth2:none").initrd_addresses();
        assert_eq!(addresses, [Ipv4Addr::new(10, 0, 0, 5)]);
    }
}
