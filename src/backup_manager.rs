//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Backup / restore of the Vreckan application configuration.
//!
//! Ports `server/backup_manager`. A backup is a `tar.gz` of the persistent
//! `/data` tree (the `data_root`). The durable configuration now lives in the
//! configuration database, so a backup includes a logical JSON dump of that
//! database (`db_dump.json`) together with the file-based material that is not
//! stored in the DB (SSL material, the app-store and autostart caches). User
//! home data under `storage/` is included only when `include_user_data` is set.
//! Transient dirs (uploads, ephemeral, the backups dir itself) are excluded.
//!
//! Restore safely extracts an archive over `data_root` (rejecting absolute
//! paths, `..` traversal and any path that escapes the root), restores the
//! database from the bundled dump, and reloads the durable in-memory state so
//! no process restart is required. Active sessions / auth tokens are
//! intentionally left untouched.

use std::io::Read;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde_json::Value;
use tar::{Archive, Builder};
use tracing::{info, warn};

use crate::catalog;
use crate::files;
use crate::session_shares;
use crate::state::AppState;
use crate::user_manager;

/// Must match the client-side chunk size (app/static/js/admin.js).
const CHUNK_SIZE: usize = 2 * 1024 * 1024; // 2 MiB

/// Sub-directories of storage/ that are transient and never worth backing up.
/// (The Python tar filter drops these; here they are documented for parity.)
#[allow(dead_code)]
const TRANSIENT_STORAGE_SUBDIRS: &[&str] = &["vreckan_uploads", "vreckan_ephemeral"];

/// Name (relative to the data root) of the logical database dump stored inside
/// a backup archive.
const DB_DUMP_ARCNAME: &str = "db_dump.json";

/// A backup/restore failure that should surface to the caller.
#[derive(Debug)]
pub struct BackupError(pub String);

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Validate a backup file name. Mirrors `_BACKUP_NAME_RE`:
/// `backup_YYYYMMDD_HHMMSS.tar.gz` or `backup_YYYYMMDD_HHMMSS_ud.tar.gz`, with
/// an optional `_<n>` suffix to break same-second collisions.
fn is_valid_backup_name(name: &str) -> bool {
    let Some(stem) = name.strip_prefix("backup_").and_then(|s| s.strip_suffix(".tar.gz")) else {
        return false;
    };
    // stem is now like "YYYYMMDD_HHMMSS", "..._ud", "..._ud_1", or "..._1".
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() < 2 || parts.len() > 4 {
        return false;
    }
    // First two parts must be 8 and 6 digits.
    if parts[0].len() != 8 || !parts[0].chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    if parts[1].len() != 6 || !parts[1].chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    // Remaining parts (if any) are "_ud" and/or a numeric collision suffix.
    for p in &parts[2..] {
        if *p == "ud" || p.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        return false;
    }
    true
}

/// The backups directory (created if missing). Mirrors `get_backup_dir`.
pub fn get_backup_dir(settings: &crate::settings::Settings) -> PathBuf {
    let path = PathBuf::from(&settings.backups_path);
    std::fs::create_dir_all(&path).ok();
    path
}

fn data_root(settings: &crate::settings::Settings) -> PathBuf {
    PathBuf::from(&settings.data_root)
}

/// True when `path` is `base` or under it. Mirrors `_is_under`.
fn is_under(path: &Path, base: &Path) -> bool {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let base = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    path == base || path.starts_with(&base)
}

/// Return a list of (absolute_path, arcname) pairs to include in the tar.
/// `arcname` is the path relative to `data_root` (POSIX separators). Mirrors
/// `_collect_entries`.
fn collect_entries(
    settings: &crate::settings::Settings,
    include_user_data: bool,
) -> Vec<(PathBuf, String)> {
    let root = data_root(settings);
    let backup_dir = get_backup_dir(settings);
    let ssl_dir = Path::new(&settings.server_private_key_path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();

    let mut candidates: Vec<PathBuf> = vec![
        root.join(DB_DUMP_ARCNAME),
        PathBuf::from(&settings.autostart_cache_path),
        PathBuf::from(&settings.app_store_cache_path),
        ssl_dir,
    ];
    if include_user_data {
        candidates.push(PathBuf::from(&settings.storage_path));
    }

    let mut entries: Vec<(PathBuf, String)> = Vec::new();
    for path in candidates {
        if !path.exists() {
            continue;
        }
        if path == backup_dir || is_under(&path, &backup_dir) {
            continue; // never back up the backups dir (recursion guard)
        }
        let rel = path
            .strip_prefix(&root)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if rel.is_empty() || rel == ".." || rel.starts_with("../") {
            continue; // not under the data root; skip for non-container layouts
        }
        entries.push((path, rel));
    }
    entries
}

/// ISO-8601 mtime of a path. Mirrors `_iso_mtime`.
fn iso_mtime(path: &Path) -> String {
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()))
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or(0);
    chrono::DateTime::from_timestamp(mtime as i64, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}

/// If retention is enabled (N > 0), delete backups beyond the N newest.
/// Mirrors `_apply_retention`.
fn apply_retention(settings: &crate::settings::Settings) {
    let keep = settings.backup_retention;
    if keep <= 0 {
        return; // 0 / unset => keep all
    }
    let backup_dir = get_backup_dir(settings);
    let Ok(files) = std::fs::read_dir(&backup_dir) else {
        return;
    };
    let mut backups: Vec<(String, std::time::SystemTime)> = Vec::new();
    for entry in files.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("backup_") && name.ends_with(".tar.gz") {
            if let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) {
                backups.push((name, mtime));
            }
        }
    }
    backups.sort_by_key(|b| std::cmp::Reverse(b.1));
    for (name, _) in backups.iter().skip(keep as usize) {
        match std::fs::remove_file(backup_dir.join(name)) {
            Ok(()) => info!("Backup retention: removed old backup {name}"),
            Err(e) => warn!("Backup retention: could not remove {name}: {e}"),
        }
    }
}

/// Create a backup archive. Mirrors `create_backup`.
pub async fn create_backup(
    state: &AppState,
    include_user_data: bool,
) -> Result<Value, BackupError> {
    let settings = &state.settings;
    let root = data_root(settings);
    let backup_dir = get_backup_dir(settings);

    let now = chrono::Local::now();
    let ts = now.format("%Y%m%d_%H%M%S").to_string();
    let suffix = if include_user_data { "_ud" } else { "" };
    let mut name = format!("backup_{ts}{suffix}.tar.gz");
    let mut counter = 0;
    while backup_dir.join(&name).exists() {
        counter += 1;
        name = format!("backup_{ts}{suffix}_{counter}.tar.gz");
    }
    let path = backup_dir.join(&name);

    // Write a consistent logical dump of the configuration database first so
    // the archive captures a single point-in-time snapshot.
    let dump_path = root.join(DB_DUMP_ARCNAME);
    state
        .db
        .dump(dump_path.to_str().unwrap_or(""))
        .await
        .map_err(|e| BackupError(e.to_string()))?;

    let result = (|| -> Result<(), BackupError> {
        let entries = collect_entries(settings, include_user_data);
        if entries.is_empty() {
            return Err(BackupError("No configuration paths found to back up.".into()));
        }
        let file = std::fs::File::create(&path)
            .map_err(|e| BackupError(format!("Could not create backup file: {e}")))?;
        // The archive contains a configuration-database dump (password
        // hashes) and SSL material: owner read/write only, created by the
        // runtime (non-root) user.
        let _ = std::fs::set_permissions(
            &path,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        );
        let mut gz = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(&mut gz);
        for (abs_path, arcname) in &entries {
            if abs_path.is_dir() {
                builder
                    .append_dir_all(arcname, abs_path)
                    .map_err(|e| BackupError(format!("Failed to add {arcname}: {e}")))?;
            } else {
                builder
                    .append_path_with_name(abs_path, arcname)
                    .map_err(|e| BackupError(format!("Failed to add {arcname}: {e}")))?;
            }
        }
        builder
            .finish()
            .map_err(|e| BackupError(format!("Failed to finish tar: {e}")))?;
        drop(builder);
        gz.try_finish()
            .map_err(|e| BackupError(format!("Failed to finish gzip: {e}")))?;
        Ok(())
    })();

    // Don't leave a stale dump in the data root between backups.
    let _ = std::fs::remove_file(&dump_path);

    result?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    info!(
        "Created backup {name} ({size} bytes, user_data={include_user_data})"
    );
    apply_retention(settings);
    Ok(serde_json::json!({
        "name": name,
        "size": size,
        "created_at": iso_mtime(&path),
        "include_user_data": include_user_data,
    }))
}

/// List the backup archives, newest first. Mirrors `list_backups`.
pub fn list_backups(settings: &crate::settings::Settings) -> Vec<Value> {
    let backup_dir = get_backup_dir(settings);
    let mut out: Vec<Value> = Vec::new();
    let Ok(files) = std::fs::read_dir(&backup_dir) else {
        return out;
    };
    for entry in files.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !(name.starts_with("backup_") && name.ends_with(".tar.gz")) {
            continue;
        }
        let path = entry.path();
        out.push(serde_json::json!({
            "name": name,
            "size": std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            "created_at": iso_mtime(&path),
            "include_user_data": name.contains("_ud"),
        }));
    }
    out.sort_by(|a, b| {
        b.get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .cmp(a.get("created_at").and_then(|v| v.as_str()).unwrap_or(""))
    });
    out
}

/// Resolve a backup name to its on-disk path, validating it. Mirrors
/// `_resolve_backup`.
pub fn resolve_backup(settings: &crate::settings::Settings, name: &str) -> Result<PathBuf, BackupError> {
    if name.is_empty() || !is_valid_backup_name(name) {
        return Err(BackupError("Invalid backup name.".into()));
    }
    let backup_dir = get_backup_dir(settings);
    let path = backup_dir.join(name);
    if !is_under(&path, &backup_dir) || !path.is_file() {
        return Err(BackupError("Backup not found.".into()));
    }
    Ok(path)
}

/// Return (raw_chunk_bytes, is_last) for the given backup and chunk index.
/// Mirrors `get_backup_chunk`.
pub fn get_backup_chunk(
    settings: &crate::settings::Settings,
    name: &str,
    chunk_index: i64,
) -> Result<(Vec<u8>, bool), BackupError> {
    let path = resolve_backup(settings, name)?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if chunk_index < 0 {
        return Err(BackupError("chunk_index must be >= 0".into()));
    }
    let start = (chunk_index as u64) * (CHUNK_SIZE as u64);
    if start >= size {
        return Ok((Vec::new(), true));
    }
    let mut f = std::fs::File::open(&path)
        .map_err(|e| BackupError(format!("Could not open backup: {e}")))?;
    use std::io::Seek;
    f.seek(std::io::SeekFrom::Start(start))
        .map_err(|e| BackupError(format!("Could not seek: {e}")))?;
    let mut buf = vec![0u8; CHUNK_SIZE];
    let n = f
        .read(&mut buf)
        .map_err(|e| BackupError(format!("Could not read chunk: {e}")))?;
    buf.truncate(n);
    let is_last = (start + n as u64) >= size;
    Ok((buf, is_last))
}

/// Delete a backup archive. Mirrors `delete_backup`.
pub fn delete_backup(settings: &crate::settings::Settings, name: &str) -> Result<(), BackupError> {
    let path = resolve_backup(settings, name)?;
    std::fs::remove_file(&path).map_err(|e| BackupError(format!("Could not delete: {e}")))?;
    info!("Deleted backup {name}");
    Ok(())
}

/// Validate every archive member: reject absolute paths, `..` traversal, and
/// any path that escapes the data root. Mirrors `_validate_archive`.
fn validate_archive<R: Read>(archive: &mut Archive<R>, root: &Path) -> Result<(), BackupError> {
    let entries = archive
        .entries()
        .map_err(|e| BackupError(format!("Bad archive: {e}")))?;
    for entry in entries {
        let entry = entry.map_err(|e| BackupError(format!("Bad archive entry: {e}")))?;
        let name = entry
            .path()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .map_err(|e| BackupError(format!("Bad archive entry path: {e}")))?;
        if name.starts_with('/') {
            return Err(BackupError(format!("Archive contains an absolute path: {name}")));
        }
        let parts: Vec<&str> = name.split('/').collect();
        if parts.contains(&"..") {
            return Err(BackupError(format!("Archive contains path traversal: {name}")));
        }
        let target = root.join(&name);
        if let Ok(canon) = target.canonicalize() {
            if let Ok(root_c) = root.canonicalize() {
                if canon != root_c && !canon.starts_with(&root_c) {
                    return Err(BackupError(format!("Archive path escapes the data root: {name}")));
                }
            }
        }
    }
    Ok(())
}

/// Safely extract an archive over the data root. Mirrors `_safe_extract`.
fn safe_extract(archive_path: &Path, root: &Path) -> Result<(), BackupError> {
    std::fs::create_dir_all(root).ok();
    let raw = std::fs::read(archive_path)
        .map_err(|e| BackupError(format!("Could not read archive: {e}")))?;
    // Validate first (on one decoder), then extract (on a fresh decoder, since
    // the cursor is consumed by validation).
    let mut gz = GzDecoder::new(std::io::Cursor::new(raw.clone()));
    let mut archive = Archive::new(&mut gz);
    validate_archive(&mut archive, root)?;
    let mut gz2 = GzDecoder::new(std::io::Cursor::new(raw));
    let mut archive2 = Archive::new(&mut gz2);
    archive2
        .unpack(root)
        .map_err(|e| BackupError(format!("Refusing to extract unsafe archive: {e}")))?;
    Ok(())
}

/// Restore the database (if the archive carried a dump) and refresh the
/// durable in-memory state. Active sessions / auth tokens are deliberately NOT
/// reloaded so a restore does not disrupt currently-connected users. Mirrors
/// `_reload_state`.
async fn reload_state(state: &AppState) {
    let root = data_root(&state.settings);
    let dump_path = root.join(DB_DUMP_ARCNAME);
    if dump_path.exists() {
        if let Err(e) = state.db.restore(dump_path.to_str().unwrap_or("")).await {
            warn!("Backup restore: could not restore database: {e}");
        } else {
            info!("Backup restore: restored configuration database.");
        }
        let _ = std::fs::remove_file(&dump_path);
    }
    // Volume mounts are read directly from the DB (no in-memory cache), so no
    // reload is needed for them.
    let _ = user_manager::load_users_and_groups(&state.db, &state.settings).await;
    catalog::load_installed_apps(state).await;
    catalog::load_app_templates(state).await;
    files::load_public_shares(state).await;
    session_shares::load_session_shares(state).await;
    info!("Backup restore: reloaded in-memory configuration.");
}

/// Restore from a stored backup archive. Mirrors `restore_from_stored`.
pub async fn restore_from_stored(state: &AppState, name: &str) -> Result<Value, BackupError> {
    let path = resolve_backup(&state.settings, name)?;
    let root = data_root(&state.settings);
    safe_extract(&path, &root)?;
    reload_state(state).await;
    info!("Restored configuration from stored backup {name}");
    Ok(serde_json::json!({ "source": name, "restored": true }))
}

/// Restore from an uploaded archive (raw bytes). Mirrors `restore_from_bytes`.
pub async fn restore_from_bytes(state: &AppState, data: &[u8]) -> Result<Value, BackupError> {
    if data.is_empty() {
        return Err(BackupError("Empty archive.".into()));
    }
    let backup_dir = get_backup_dir(&state.settings);
    let tmp = backup_dir.join(format!(".restore_tmp_{}.tar.gz", crate::catalog::random_uuid()));
    std::fs::write(&tmp, data).map_err(|e| BackupError(format!("Could not write temp: {e}")))?;
    let result = (|| -> Result<Value, BackupError> {
        let root = data_root(&state.settings);
        safe_extract(&tmp, &root)?;
        Ok(serde_json::json!({ "source": "upload", "restored": true }))
    })();
    let _ = std::fs::remove_file(&tmp);
    let value = result?;
    reload_state(state).await;
    info!("Restored configuration from uploaded archive ({} bytes)", data.len());
    Ok(value)
}
