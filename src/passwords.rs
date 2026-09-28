// Model-output: Claude Opus 5.5

//! Machines' LUKS passwords, stored in files encrypted with a key derived
//! from an SSH signature.
//!
//! The key is a hash of `ssh-keygen -Y sign` over a fixed message, so it's
//! available wherever the user's SSH key is (e.g. through their agent).  That
//! only works with keys whose signatures are deterministic, like Ed25519 and
//! RSA keys but unlike ECDSA and FIDO keys; [`save`] checks.

use crate::child;
use crate::config::check_hostname;
use crate::deadline::Deadline;
use crate::initrd::check_password;
use anyhow::{Context, Result, anyhow, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// What gets signed.
const MESSAGE: &[u8] = b"reboop-luks-v1";
/// The ssh-keygen signature namespace, so this signature can't be mistaken
/// for one made for another purpose.
const NAMESPACE: &str = "reboop-luks";
/// Starts every password file, and is authenticated along with the hostname.
const MAGIC: &[u8] = b"reboop-luks-v1\n";
const NONCE_LEN: usize = 24;

/// A 256-bit key for encrypting password files.
#[derive(Clone, PartialEq, Eq)]
pub struct MasterKey([u8; 32]);

/// Decodes an armored SSH signature ("-----BEGIN SSH SIGNATURE-----"...).
fn decode_armor(armored: &str) -> Result<Vec<u8>> {
    let body = armored
        .trim()
        .strip_prefix("-----BEGIN SSH SIGNATURE-----")
        .and_then(|rest| rest.strip_suffix("-----END SSH SIGNATURE-----"))
        .ok_or_else(|| anyhow!("unexpected ssh-keygen output {armored:?}"))?;
    let base64: String = body.split_whitespace().collect();
    let blob = BASE64.decode(base64).context("bad base64 in ssh-keygen output")?;
    ensure!(blob.starts_with(b"SSHSIG"), "ssh-keygen output isn't an SSH signature");
    Ok(blob)
}

/// Derives the master key by signing with `signing_key`: a private key file,
/// or a public key file whose private half is in ssh-agent.
pub fn master_key(signing_key: &Path) -> Result<MasterKey> {
    let mut command = Command::new("ssh-keygen");
    // hashalg is the default, but pinned so the key can't change if the default does.
    command.args(["-Y", "sign", "-n", NAMESPACE, "-O", "hashalg=sha512", "-f"]).arg(signing_key);
    // Generous, in case the agent asks the user to confirm.
    let output = child::run(command, MESSAGE, Deadline::after(Duration::from_secs(120)))?;
    ensure!(
        output.status.success(),
        "ssh-keygen -Y sign with {} failed: {}",
        signing_key.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let signature = decode_armor(&String::from_utf8_lossy(&output.stdout))?;
    let hash = Sha256::new().chain_update(b"reboop-luks-v1 master key\0").chain_update(&signature).finalize();
    Ok(MasterKey(hash.into()))
}

/// Where the password for machine `hostname` is kept in `dir`.
pub fn password_file(dir: &Path, hostname: &str) -> Result<PathBuf> {
    check_hostname(hostname)?;
    Ok(dir.join(hostname))
}

/// Additional authenticated data, which binds a file to its machine.
fn aad(hostname: &str) -> Vec<u8> {
    [MAGIC, hostname.as_bytes()].concat()
}

/// Encrypts `password` for machine `hostname` into the contents of a
/// password file: [`MAGIC`], a random nonce, then the XChaCha20-Poly1305
/// ciphertext (with its tag).
fn encrypt(key: &MasterKey, hostname: &str, password: &str) -> Result<Vec<u8>> {
    let mut nonce = [0; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|error| anyhow!("failed to get random bytes: {error}"))?;
    let cipher = XChaCha20Poly1305::new(&key.0.into());
    let payload = Payload { msg: password.as_bytes(), aad: &aad(hostname) };
    let ciphertext = cipher.encrypt(&XNonce::from(nonce), payload).map_err(|_| anyhow!("encryption failed"))?;
    Ok([MAGIC, &nonce, &ciphertext].concat())
}

/// Decrypts the `contents` of machine `hostname`'s password file (see
/// [`encrypt`]).
fn decrypt(key: &MasterKey, hostname: &str, contents: &[u8]) -> Result<String> {
    let rest = contents.strip_prefix(MAGIC).ok_or_else(|| anyhow!("not a reboop password file"))?;
    ensure!(rest.len() > NONCE_LEN, "truncated password file");
    let (nonce, ciphertext) = rest.split_at(NONCE_LEN);
    let nonce = XNonce::try_from(nonce).expect("NONCE_LEN is the nonce length");
    let cipher = XChaCha20Poly1305::new(&key.0.into());
    let payload = Payload { msg: ciphertext, aad: &aad(hostname) };
    let plaintext = cipher
        .decrypt(&nonce, payload)
        .map_err(|_| anyhow!("couldn't decrypt the password for {hostname}: wrong SSH key, or the file isn't this machine's"))?;
    String::from_utf8(plaintext).context("the decrypted password isn't UTF-8")
}

/// Replaces `path` with a mode-0600 file holding `contents`, via a temporary
/// file named ".NAME.tmp", such that a crash leaves either the old file or
/// the new one.
fn write_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let name = path.file_name().ok_or_else(|| anyhow!("{} has no file name", path.display()))?;
    let mut temporary_name = OsString::from(".");
    temporary_name.push(name);
    temporary_name.push(".tmp");
    let temporary = path.with_file_name(temporary_name);
    let mut file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&temporary)?;
    // In case a stale temporary file had other permissions.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()?;
    Ok(())
}

/// Encrypts `password` for machine `hostname` into a file in `dir`,
/// replacing any previous one.  Refuses keys that can't reliably decrypt it
/// again: signs twice and requires the same signature.
pub fn save(dir: &Path, hostname: &str, password: &str, signing_key: &Path) -> Result<()> {
    check_password(password)?;
    let path = password_file(dir, hostname)?;
    let key = master_key(signing_key)?;
    ensure!(
        master_key(signing_key)? == key,
        "{} makes a different signature every time (is it an ECDSA or FIDO key?), so it can't protect passwords",
        signing_key.display()
    );

    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    // The temporary file can't be another machine's, as hostnames don't
    // start with a dot.
    write_atomically(&path, &encrypt(&key, hostname, password)?)?;
    let saved = decrypt(&key, hostname, &fs::read(&path)?)?;
    ensure!(saved == password, "{} doesn't hold the password after writing it", path.display());
    Ok(())
}

/// Decrypts the password for machine `hostname` from its file in `dir`.
pub fn load(dir: &Path, hostname: &str, signing_key: &Path) -> Result<String> {
    let path = password_file(dir, hostname)?;
    let contents = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    decrypt(&master_key(signing_key)?, hostname, &contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keygen(dir: &Path, name: &str, key_type: &str) -> PathBuf {
        let path = dir.join(name);
        let status = Command::new("ssh-keygen").args(["-q", "-N", "", "-t", key_type, "-f"]).arg(&path).status().unwrap();
        assert!(status.success());
        path
    }

    #[test]
    fn saves_and_loads() {
        let temp = tempfile::tempdir().unwrap();
        let key = keygen(temp.path(), "id_ed25519", "ed25519");
        let dir = temp.path().join("luks");
        save(&dir, "one", "correct horse battery staple", &key).unwrap();
        assert_eq!(load(&dir, "one", &key).unwrap(), "correct horse battery staple");
        let mode = fs::metadata(dir.join("one")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // Another machine's file, or another key, doesn't decrypt.
        fs::copy(dir.join("one"), dir.join("two")).unwrap();
        assert!(load(&dir, "two", &key).is_err());
        let other_key = keygen(temp.path(), "other", "ed25519");
        assert!(load(&dir, "one", &other_key).is_err());

        // Saving doesn't disturb the file of a machine with a similar name.
        save(&dir, "one.tmp", "another password", &key).unwrap();
        save(&dir, "one", "correct horse battery staple", &key).unwrap();
        assert_eq!(load(&dir, "one.tmp", &key).unwrap(), "another password");

        // Nor does a modified file.
        let mut contents = fs::read(dir.join("one")).unwrap();
        *contents.last_mut().unwrap() ^= 1;
        fs::write(dir.join("one"), contents).unwrap();
        assert!(load(&dir, "one", &key).is_err());
    }

    #[test]
    fn refuses_nondeterministic_keys() {
        let temp = tempfile::tempdir().unwrap();
        let key = keygen(temp.path(), "id_ecdsa", "ecdsa");
        let error = save(&temp.path().join("luks"), "one", "password", &key).unwrap_err();
        assert!(error.to_string().contains("different signature"), "{error:#}");
    }

    #[test]
    fn refuses_bad_hostnames_and_passwords() {
        let temp = tempfile::tempdir().unwrap();
        let key = keygen(temp.path(), "id_ed25519", "ed25519");
        assert!(save(temp.path(), "../evil", "password", &key).is_err());
        assert!(save(temp.path(), "one", "pass\nword", &key).is_err());
    }
}
