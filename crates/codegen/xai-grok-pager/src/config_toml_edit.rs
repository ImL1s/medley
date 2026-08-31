//! Load `config.toml` as a [`toml_edit::DocumentMut`] for in-place edits.
//! A non-empty file that does not parse is left untouched (`None`).

use std::fs::File;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigMutationSnapshot {
    pub generation: u64,
    pub byte_digest: String,
    pub overlay_digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigMutationPersistence {
    PersistedForNewSessions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigMutationOutcome {
    pub generation: u64,
    pub byte_digest: String,
    pub overlay_digest: String,
    pub persistence: ConfigMutationPersistence,
    pub active_session_changed: bool,
}

#[derive(Debug)]
pub(crate) enum ConfigMutationError {
    StaleGeneration { rendered: u64, current: u64 },
    ConcurrentEdit,
    Read(std::io::Error),
    Malformed(String),
    Cancelled(String),
    Write(std::io::Error),
    Readback(std::io::Error),
    ReadbackMismatch,
    Rollback(std::io::Error),
}

impl std::fmt::Display for ConfigMutationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleGeneration { rendered, current } => {
                write!(
                    f,
                    "stale generation {rendered}; current generation is {current}"
                )
            }
            Self::ConcurrentEdit => write!(f, "config.toml changed after it was rendered"),
            Self::Read(error) => write!(f, "failed to read config.toml: {error}"),
            Self::Malformed(error) => write!(f, "config.toml is not valid TOML: {error}"),
            Self::Cancelled(error) => write!(f, "mutation cancelled: {error}"),
            Self::Write(error) => write!(f, "failed to atomically write config.toml: {error}"),
            Self::Readback(error) => write!(f, "failed to read back config.toml: {error}"),
            Self::ReadbackMismatch => write!(f, "config.toml readback did not match written bytes"),
            Self::Rollback(error) => write!(f, "failed to restore config.toml: {error}"),
        }
    }
}

fn digest_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn read_config_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

pub(crate) fn lock_config_destination(path: &Path) -> std::io::Result<(File, std::path::PathBuf)> {
    xai_grok_config::fs_atomic::lock_config_destination(path)
}

pub(crate) fn write_config_toml(path: &Path, contents: &str) -> std::io::Result<()> {
    let mode = destination_unix_mode(path);
    xai_grok_config::fs_atomic::write_atomically_at(path, contents, mode)
}

/// Read config text, treating only `NotFound` as empty. Hard errors (`EACCES`)
/// propagate so callers cannot replace an unreadable file.
pub(crate) fn read_config_text(path: &Path) -> std::io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error),
    }
}

fn overlay_digest_for(path: &Path) -> String {
    let mut payload = Vec::new();
    let mut dirs = Vec::new();
    if let Some(parent) = path.parent() {
        dirs.push(parent.to_path_buf());
    }
    if let Some(system) = xai_grok_config::system_config_dir() {
        dirs.push(system);
    }
    for dir in dirs {
        for name in [
            xai_grok_config::MANAGED_CONFIG_FILENAME,
            xai_grok_config::REQUIREMENTS_FILENAME,
            "campaigns_state.json",
        ] {
            match std::fs::read(dir.join(name)) {
                Ok(bytes) => {
                    payload.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                    payload.extend_from_slice(&bytes);
                }
                Err(_) => payload.extend_from_slice(&0u64.to_le_bytes()),
            }
        }
    }
    let remote = xai_grok_shell::util::config::remote_campaign_cache_fingerprint();
    payload.extend_from_slice(&(remote.len() as u64).to_le_bytes());
    payload.extend_from_slice(&remote);
    let mdm = xai_grok_config::mdm_requirements_fingerprint();
    payload.extend_from_slice(&(mdm.len() as u64).to_le_bytes());
    payload.extend_from_slice(&mdm);
    digest_bytes(&payload)
}

fn destination_unix_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        Some(match std::fs::metadata(path) {
            Ok(metadata) => metadata.permissions().mode() & 0o777,
            Err(_) => 0o600,
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

pub(crate) fn config_mutation_snapshot(
    path: &Path,
    generation: u64,
) -> Result<ConfigMutationSnapshot, ConfigMutationError> {
    let bytes = read_config_bytes(path).map_err(ConfigMutationError::Read)?;
    Ok(ConfigMutationSnapshot {
        generation,
        byte_digest: digest_bytes(&bytes),
        overlay_digest: overlay_digest_for(path),
    })
}

/// Apply an edit only to the exact bytes and generation that were rendered.
/// The write is atomic and verified by exact readback. A failed verification
/// restores the original bytes before returning an error.
pub(crate) fn mutate_config_document_at<F>(
    path: &Path,
    rendered: &ConfigMutationSnapshot,
    current_generation: u64,
    edit: F,
) -> Result<ConfigMutationOutcome, ConfigMutationError>
where
    F: FnOnce(&mut toml_edit::DocumentMut) -> Result<(), String>,
{
    let (_lock, dest) = xai_grok_config::fs_atomic::lock_config_destination(path)
        .map_err(ConfigMutationError::Write)?;
    if overlay_digest_for(path) != rendered.overlay_digest {
        return Err(ConfigMutationError::ConcurrentEdit);
    }
    let mode = destination_unix_mode(&dest);
    let mut outcome = mutate_config_document_at_with(
        &dest,
        path,
        rendered,
        current_generation,
        edit,
        read_config_bytes,
        move |path, contents| xai_grok_config::fs_atomic::write_atomically_at(path, contents, mode),
    )?;
    outcome.overlay_digest = overlay_digest_for(path);
    Ok(outcome)
}

fn restore_original_if_unchanged<R, W>(
    path: &Path,
    original: &[u8],
    intended: &[u8],
    read: &mut R,
    write: &mut W,
) -> Result<bool, ConfigMutationError>
where
    R: FnMut(&Path) -> std::io::Result<Vec<u8>>,
    W: FnMut(&Path, &str) -> std::io::Result<()>,
{
    match read(path) {
        Ok(bytes) if bytes == intended => {
            let original = std::str::from_utf8(original).expect("validated UTF-8 above");
            write(path, original).map_err(ConfigMutationError::Rollback)?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(error) => Err(ConfigMutationError::Rollback(error)),
    }
}

fn mutate_config_document_at_with<F, R, W>(
    path: &Path,
    overlay_path: &Path,
    rendered: &ConfigMutationSnapshot,
    current_generation: u64,
    edit: F,
    mut read: R,
    mut write: W,
) -> Result<ConfigMutationOutcome, ConfigMutationError>
where
    F: FnOnce(&mut toml_edit::DocumentMut) -> Result<(), String>,
    R: FnMut(&Path) -> std::io::Result<Vec<u8>>,
    W: FnMut(&Path, &str) -> std::io::Result<()>,
{
    if rendered.generation != current_generation {
        return Err(ConfigMutationError::StaleGeneration {
            rendered: rendered.generation,
            current: current_generation,
        });
    }
    let original = read(path).map_err(ConfigMutationError::Read)?;
    if digest_bytes(&original) != rendered.byte_digest {
        return Err(ConfigMutationError::ConcurrentEdit);
    }
    let source = std::str::from_utf8(&original)
        .map_err(|error| ConfigMutationError::Malformed(error.to_string()))?;
    let mut document = if source.is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        source.parse().map_err(|error: toml_edit::TomlError| {
            ConfigMutationError::Malformed(error.to_string())
        })?
    };
    edit(&mut document).map_err(ConfigMutationError::Cancelled)?;
    let intended = document.to_string();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(ConfigMutationError::Write)?;
    }
    let current = read(path).map_err(ConfigMutationError::Read)?;
    if current != original {
        return Err(ConfigMutationError::ConcurrentEdit);
    }
    write(path, &intended).map_err(ConfigMutationError::Write)?;
    match read(path) {
        Ok(bytes) if bytes == intended.as_bytes() => {
            if overlay_digest_for(overlay_path) != rendered.overlay_digest {
                restore_original_if_unchanged(
                    path,
                    &original,
                    intended.as_bytes(),
                    &mut read,
                    &mut write,
                )?;
                return Err(ConfigMutationError::ConcurrentEdit);
            }
            Ok(ConfigMutationOutcome {
                generation: current_generation,
                byte_digest: digest_bytes(&bytes),
                overlay_digest: overlay_digest_for(overlay_path),
                persistence: ConfigMutationPersistence::PersistedForNewSessions,
                active_session_changed: false,
            })
        }
        Ok(bytes) if bytes.as_slice() == original.as_slice() => {
            Err(ConfigMutationError::ReadbackMismatch)
        }
        Ok(_) => Err(ConfigMutationError::ConcurrentEdit),
        Err(error) => match read(path) {
            Ok(bytes) if bytes == intended.as_bytes() => {
                let restored = restore_original_if_unchanged(
                    path,
                    &original,
                    intended.as_bytes(),
                    &mut read,
                    &mut write,
                )?;
                if restored {
                    Err(ConfigMutationError::Readback(error))
                } else {
                    Err(ConfigMutationError::ConcurrentEdit)
                }
            }
            Ok(bytes) if bytes.as_slice() == original.as_slice() => {
                Err(ConfigMutationError::Readback(error))
            }
            Ok(_) => Err(ConfigMutationError::ConcurrentEdit),
            Err(_) => Err(ConfigMutationError::Readback(error)),
        },
    }
}

/// Load `config.toml` for a read-modify-write. `Ok(None)` means the file is
/// non-empty but unparseable (callers must not overwrite). Hard read errors
/// other than `NotFound` are returned so an unreadable file is not replaced.
pub(crate) fn read_config_document_for_edit(
    path: &Path,
) -> std::io::Result<Option<toml_edit::DocumentMut>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    match content.parse() {
        Ok(d) => Ok(Some(d)),
        Err(e) => {
            if content.is_empty() {
                return Ok(Some(toml_edit::DocumentMut::new()));
            }
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "config.toml is not valid TOML; refusing to overwrite"
            );
            Ok(None)
        }
    }
}

/// Set `[hints].<key>` to `value` in `~/.grok/config.toml`, preserving every
/// other key and table. Creates the file and parent dir when missing, and
/// no-ops when the existing file is non-empty but unparseable (so a malformed
/// config is never clobbered). Performs blocking I/O.
pub(crate) fn set_hint(key: &str, value: impl Into<toml_edit::Value>) -> std::io::Result<()> {
    let path = xai_grok_tools::util::grok_home::grok_home().join("config.toml");
    set_hint_at(&path, key, value)
}

/// Path-injectable core of [`set_hint`].
fn set_hint_at(path: &Path, key: &str, value: impl Into<toml_edit::Value>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let (_lock, dest) = xai_grok_config::fs_atomic::lock_config_destination(path)?;
    let Some(mut doc) = read_config_document_for_edit(&dest)? else {
        return Ok(());
    };
    doc["hints"][key] = toml_edit::value(value);
    write_config_toml(&dest, &doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::io;
    use tempfile::tempdir;

    fn set_agent_enabled(document: &mut toml_edit::DocumentMut) -> Result<(), String> {
        document["subagents"]["toggle"]["verifier"] = toml_edit::value(true);
        Ok(())
    }

    #[test]
    fn transaction_refuses_stale_generation_without_touching_bytes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "[ui]\ntheme = \"dark\"\n";
        fs::write(&path, original).unwrap();
        let rendered = config_mutation_snapshot(&path, 7).unwrap();

        let error = mutate_config_document_at(&path, &rendered, 8, set_agent_enabled).unwrap_err();

        assert!(matches!(error, ConfigMutationError::StaleGeneration { .. }));
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn transaction_refuses_concurrent_byte_edit_without_overwrite() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[ui]\ntheme = \"dark\"\n").unwrap();
        let rendered = config_mutation_snapshot(&path, 3).unwrap();
        let concurrent = "# concurrent\n[ui]\ntheme = \"light\"\n";
        fs::write(&path, concurrent).unwrap();

        let error = mutate_config_document_at(&path, &rendered, 3, set_agent_enabled).unwrap_err();

        assert!(matches!(error, ConfigMutationError::ConcurrentEdit));
        assert_eq!(fs::read_to_string(path).unwrap(), concurrent);
    }

    #[test]
    fn transaction_rechecks_bytes_before_replace() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "[ui]\ntheme = \"dark\"\n";
        fs::write(&path, original).unwrap();
        let rendered = config_mutation_snapshot(&path, 1).unwrap();
        let reads = Cell::new(0);
        let writes = Cell::new(0);
        let error = mutate_config_document_at_with(
            &path,
            &path,
            &rendered,
            1,
            set_agent_enabled,
            |path| {
                let n = reads.get();
                reads.set(n + 1);
                if n == 1 {
                    return Ok(b"# editor\n[ui]\ntheme = \"light\"\n".to_vec());
                }
                fs::read(path)
            },
            |path, contents| {
                writes.set(writes.get() + 1);
                fs::write(path, contents)
            },
        )
        .unwrap_err();
        assert!(matches!(error, ConfigMutationError::ConcurrentEdit));
        assert_eq!(writes.get(), 0, "must not replace after a stale recheck");
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn transaction_read_parse_write_and_cancel_failures_preserve_bytes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "[ui]\ntheme = \"dark\"\n";
        fs::write(&path, original).unwrap();
        let rendered = config_mutation_snapshot(&path, 1).unwrap();

        let cancelled =
            mutate_config_document_at(&path, &rendered, 1, |_| Err("cancel".into())).unwrap_err();
        assert!(matches!(cancelled, ConfigMutationError::Cancelled(_)));

        let read_error = mutate_config_document_at_with(
            &path,
            &path,
            &rendered,
            1,
            set_agent_enabled,
            |_| Err(io::Error::other("read failed")),
            |_, _| panic!("write must not run"),
        )
        .unwrap_err();
        assert!(matches!(read_error, ConfigMutationError::Read(_)));

        let write_error = mutate_config_document_at_with(
            &path,
            &path,
            &rendered,
            1,
            set_agent_enabled,
            |path| fs::read(path),
            |_, _| Err(io::Error::other("write failed")),
        )
        .unwrap_err();
        assert!(matches!(write_error, ConfigMutationError::Write(_)));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);

        fs::write(&path, "[broken\n").unwrap();
        let malformed_snapshot = config_mutation_snapshot(&path, 1).unwrap();
        let malformed = mutate_config_document_at(&path, &malformed_snapshot, 1, set_agent_enabled)
            .unwrap_err();
        assert!(matches!(malformed, ConfigMutationError::Malformed(_)));
        assert_eq!(fs::read_to_string(path).unwrap(), "[broken\n");
    }

    #[test]
    fn transaction_readback_failure_rolls_back_original_bytes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "# keep\n[capability]\nmode = \"read-only\"\n";
        fs::write(&path, original).unwrap();
        let rendered = config_mutation_snapshot(&path, 2).unwrap();
        let reads = Cell::new(0);

        let error = mutate_config_document_at_with(
            &path,
            &path,
            &rendered,
            2,
            set_agent_enabled,
            |path| {
                let call = reads.get();
                reads.set(call + 1);
                if call == 1 {
                    Err(io::Error::other("readback failed"))
                } else {
                    fs::read(path)
                }
            },
            |path, contents| xai_grok_config::fs_atomic::write_atomically_at(path, contents, None),
        )
        .unwrap_err();

        assert!(matches!(error, ConfigMutationError::Readback(_)));
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn transaction_preserves_comments_siblings_capability_and_session_scope() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "# user comment\n[ui]\ntheme = \"dark\"\n\n[capability]\nmode = \"read-only\"\n",
        )
        .unwrap();
        let rendered = config_mutation_snapshot(&path, 9).unwrap();

        let outcome = mutate_config_document_at(&path, &rendered, 9, set_agent_enabled).unwrap();
        let body = fs::read_to_string(&path).unwrap();

        assert!(body.contains("# user comment"));
        assert!(body.contains("theme = \"dark\""));
        assert!(body.contains("mode = \"read-only\""));
        assert!(body.contains("verifier = true"));
        assert_eq!(outcome.generation, 9);
        assert_eq!(
            outcome.persistence,
            ConfigMutationPersistence::PersistedForNewSessions
        );
        assert!(!outcome.active_session_changed);
        assert_eq!(outcome.byte_digest, digest_bytes(body.as_bytes()));
    }

    #[test]
    fn transaction_second_mutation_uses_post_write_digest() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[ui]\ntheme = \"dark\"\n").unwrap();
        let first = config_mutation_snapshot(&path, 4).unwrap();
        let outcome = mutate_config_document_at(&path, &first, 4, set_agent_enabled).unwrap();
        let second = ConfigMutationSnapshot {
            generation: 4,
            byte_digest: outcome.byte_digest,
            overlay_digest: outcome.overlay_digest,
        };
        let outcome = mutate_config_document_at(&path, &second, 4, |document| {
            document["agent"]["name"] = toml_edit::value("explore");
            Ok(())
        })
        .unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("verifier = true"));
        assert!(body.contains("name = \"explore\""));
        assert_eq!(outcome.byte_digest, digest_bytes(body.as_bytes()));
    }

    #[test]
    fn transaction_does_not_restore_original_over_a_newer_writer() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "[ui]\ntheme = \"dark\"\n";
        let concurrent = "# concurrent\n[ui]\ntheme = \"light\"\n";
        fs::write(&path, original).unwrap();
        let rendered = config_mutation_snapshot(&path, 1).unwrap();
        let error = mutate_config_document_at_with(
            &path,
            &path,
            &rendered,
            1,
            set_agent_enabled,
            |path| fs::read(path),
            |path, _contents| fs::write(path, concurrent),
        )
        .unwrap_err();
        assert!(matches!(error, ConfigMutationError::ConcurrentEdit));
        assert_eq!(fs::read_to_string(path).unwrap(), concurrent);
    }

    #[cfg(unix)]
    #[test]
    fn transaction_preserves_existing_unix_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[ui]\ntheme = \"dark\"\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let rendered = config_mutation_snapshot(&path, 2).unwrap();

        mutate_config_document_at(&path, &rendered, 2, set_agent_enabled).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn transaction_writes_through_symlink_target() {
        let dir = tempdir().unwrap();
        let target_dir = dir.path().join("dotfiles");
        fs::create_dir(&target_dir).unwrap();
        let target = target_dir.join("config.toml");
        let path = dir.path().join("config.toml");
        fs::write(&target, "[ui]\ntheme = \"dark\"\n").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let rendered = config_mutation_snapshot(&path, 1).unwrap();

        mutate_config_document_at(&path, &rendered, 1, set_agent_enabled).unwrap();

        assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
        let body = fs::read_to_string(&target).unwrap();
        assert!(body.contains("verifier = true"));
        assert!(body.contains("theme = \"dark\""));
    }

    #[test]
    fn overlay_digest_includes_mdm_requirements_fingerprint() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[ui]\ntheme = \"dark\"\n").unwrap();
        let first = overlay_digest_for(&path);
        let second = overlay_digest_for(&path);
        assert_eq!(first, second);
        assert!(!xai_grok_config::mdm_requirements_fingerprint().is_empty());
    }

    #[test]
    fn transaction_rejects_overlay_change_as_concurrent_edit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[ui]\ntheme = \"dark\"\n").unwrap();
        let rendered = config_mutation_snapshot(&path, 1).unwrap();
        fs::write(
            dir.path().join(xai_grok_config::MANAGED_CONFIG_FILENAME),
            "[subagents.toggle]\nexplore = false\n",
        )
        .unwrap();
        let error = mutate_config_document_at(&path, &rendered, 1, set_agent_enabled).unwrap_err();
        assert!(matches!(error, ConfigMutationError::ConcurrentEdit));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[ui]\ntheme = \"dark\"\n"
        );
    }

    #[test]
    fn transaction_rolls_back_when_overlay_changes_during_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "[ui]\ntheme = \"dark\"\n";
        fs::write(&path, original).unwrap();
        let rendered = config_mutation_snapshot(&path, 1).unwrap();
        let campaign = dir.path().join("campaigns_state.json");
        let error = mutate_config_document_at_with(
            &path,
            &path,
            &rendered,
            1,
            set_agent_enabled,
            |path| fs::read(path),
            |path, contents| {
                fs::write(&campaign, "{\"dismissed_ids\":[\"camp-1\"]}\n").unwrap();
                fs::write(path, contents)
            },
        )
        .unwrap_err();
        assert!(matches!(error, ConfigMutationError::ConcurrentEdit));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn transaction_does_not_rollback_over_a_newer_editor_after_overlay_mismatch() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "[ui]\ntheme = \"dark\"\n";
        fs::write(&path, original).unwrap();
        let rendered = config_mutation_snapshot(&path, 1).unwrap();
        let campaign = dir.path().join("campaigns_state.json");
        let reads = Cell::new(0);
        let error = mutate_config_document_at_with(
            &path,
            &path,
            &rendered,
            1,
            set_agent_enabled,
            |path| {
                let n = reads.get();
                reads.set(n + 1);
                if n >= 2 {
                    return Ok(b"newer-from-editor\n".to_vec());
                }
                fs::read(path)
            },
            |path, contents| {
                fs::write(&campaign, "{\"dismissed_ids\":[\"camp-1\"]}\n").unwrap();
                fs::write(path, contents)
            },
        )
        .unwrap_err();
        assert!(matches!(error, ConfigMutationError::ConcurrentEdit));
        let body = fs::read_to_string(&path).unwrap();
        assert_ne!(body, original);
        assert!(
            body.contains("verifier"),
            "newer editor reread must skip rollback: {body}"
        );
    }

    #[test]
    fn transaction_rejects_campaign_state_change_as_concurrent_edit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[ui]\ntheme = \"dark\"\n").unwrap();
        let rendered = config_mutation_snapshot(&path, 1).unwrap();
        fs::write(
            dir.path().join("campaigns_state.json"),
            "{\"dismissed_ids\":[\"camp-1\"]}\n",
        )
        .unwrap();
        let error = mutate_config_document_at(&path, &rendered, 1, set_agent_enabled).unwrap_err();
        assert!(matches!(error, ConfigMutationError::ConcurrentEdit));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[ui]\ntheme = \"dark\"\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn transaction_creates_missing_file_as_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let rendered = config_mutation_snapshot(&path, 1).unwrap();

        mutate_config_document_at(&path, &rendered, 1, set_agent_enabled).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn merge_round_trip_preserves_sibling_tables() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[ui]\ncompact_mode = false\n\n[mcpServers]\nx = \"y\"\n",
        )
        .unwrap();

        let mut doc = read_config_document_for_edit(&path)
            .expect("read")
            .expect("parse");
        doc["ui"]["show_timestamps"] = toml_edit::value(false);
        fs::write(&path, doc.to_string()).unwrap();

        let body = fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("show_timestamps") && body.contains("mcpServers"),
            "expected merged TOML, got:\n{body}"
        );
    }

    #[test]
    fn nonempty_unparseable_returns_none_and_leaves_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let bad = "this is [not valid toml\n";
        fs::write(&path, bad).unwrap();

        assert!(
            read_config_document_for_edit(&path)
                .expect("read")
                .is_none()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), bad);
    }

    #[test]
    fn missing_file_is_editable_empty_doc() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("absent.toml");
        let doc = read_config_document_for_edit(&path)
            .expect("read")
            .expect("editable");
        assert!(!doc.contains_key("ui"));
    }

    #[test]
    fn set_hint_at_round_trips_and_preserves_siblings() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[ui]\ncompact_mode = false\n").unwrap();

        set_hint_at(&path, "memory_modal_fullscreen", true).unwrap();

        let doc = read_config_document_for_edit(&path)
            .expect("read")
            .expect("reparse");
        assert_eq!(
            doc.get("hints")
                .and_then(|h| h.get("memory_modal_fullscreen"))
                .and_then(|v| v.as_bool()),
            Some(true),
        );
        assert!(
            fs::read_to_string(&path).unwrap().contains("compact_mode"),
            "sibling [ui] should be preserved"
        );
    }

    #[test]
    fn set_hint_at_creates_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");
        set_hint_at(&path, "memory_modal_fullscreen", true).unwrap();
        assert!(
            path.exists(),
            "missing file and parent dir should be created"
        );
    }

    #[test]
    fn set_hint_write_then_read_back_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[ui]\ntheme = \"dark\"\n").unwrap();

        set_hint_at(&path, "memory_modal_fullscreen", true).unwrap();

        let doc = read_config_document_for_edit(&path)
            .expect("read")
            .expect("reparse");
        let disabled = doc
            .get("hints")
            .and_then(|h| h.get("memory_modal_fullscreen"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(disabled, "should read back true after set_hint write");
    }

    #[test]
    fn set_hint_at_uses_shared_lock_and_preserves_unix_mode() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = tempdir().unwrap();
            let path = dir.path().join("config.toml");
            fs::write(&path, "[ui]\ntheme = \"dark\"\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            set_hint_at(&path, "memory_modal_fullscreen", true).unwrap();
            let body = fs::read_to_string(&path).unwrap();
            assert!(body.contains("theme = \"dark\""));
            assert!(body.contains("memory_modal_fullscreen"));
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            assert!(xai_grok_config::fs_atomic::config_lock_path(&path).is_file());
        }
        #[cfg(not(unix))]
        {
            let dir = tempdir().unwrap();
            let path = dir.path().join("config.toml");
            fs::write(&path, "[ui]\ntheme = \"dark\"\n").unwrap();
            set_hint_at(&path, "memory_modal_fullscreen", true).unwrap();
            assert!(xai_grok_config::fs_atomic::config_lock_path(&path).is_file());
        }
    }

    #[test]
    fn set_hint_at_leaves_unparseable_file_untouched() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let bad = "this is [not valid toml\n";
        fs::write(&path, bad).unwrap();

        // No-op (no write, no clobber) when the existing file cannot be parsed.
        set_hint_at(&path, "memory_modal_fullscreen", true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), bad);
    }

    #[cfg(unix)]
    #[test]
    fn set_hint_at_propagates_unreadable_file_without_replace() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "[ui]\ncompact_mode = false\n";
        fs::write(&path, original).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_to_string(&path).is_ok() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            return;
        }
        let err = set_hint_at(&path, "memory_modal_fullscreen", true).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn read_config_text_propagates_unreadable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[ui]\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_to_string(&path).is_ok() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            return;
        }
        let err = read_config_text(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "[ui]\n");
    }

    #[cfg(unix)]
    #[test]
    fn write_config_toml_keeps_pinned_destination_verbatim() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("config.toml");
        let other = dir.path().join("other.toml");
        fs::write(&dest, "orig\n").unwrap();
        fs::write(&other, "other\n").unwrap();
        fs::remove_file(&dest).unwrap();
        std::os::unix::fs::symlink(&other, &dest).unwrap();
        write_config_toml(&dest, "[ui]\ncompact_mode = true\n").unwrap();
        assert!(
            !dest.symlink_metadata().unwrap().file_type().is_symlink(),
            "pinned write must not follow a dest replaced with a symlink"
        );
        assert!(fs::read_to_string(&dest).unwrap().contains("compact_mode"));
        assert_eq!(fs::read_to_string(&other).unwrap(), "other\n");
    }

    #[test]
    fn vim_mode_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[ui]\ncompact_mode = false\n").unwrap();

        let mut doc = read_config_document_for_edit(&path)
            .expect("read")
            .expect("parse");
        doc["ui"]["vim_mode"] = toml_edit::value(true);
        fs::write(&path, doc.to_string()).unwrap();

        let doc2 = read_config_document_for_edit(&path)
            .expect("read")
            .expect("reparse");
        let enabled = doc2
            .get("ui")
            .and_then(|h| h.get("vim_mode"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(enabled, "expected vim_mode = true after round-trip");

        let body = fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("compact_mode"),
            "sibling [ui] keys should be preserved"
        );
    }
}
