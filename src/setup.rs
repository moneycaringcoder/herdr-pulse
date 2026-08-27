//! One-click sidebar setup.
//!
//! herdr renders a plugin's custom tokens only if the user's `config.toml` names
//! them, so without this the badge silently never appears. Rather than asking
//! people to hand-merge TOML, `--setup` splices the token entries into their
//! existing `[ui.sidebar.spaces]` rows, reloads herdr's config, and keeps an
//! exact transaction record for safe rollback.
//!
//! Safety rules this module holds to, because it edits a file it does not own:
//!
//!   * backup publication never clobbers an existing recovery point;
//!   * every config replacement is same-directory, synced, and atomic;
//!   * reload rejection restores the prior complete file;
//!   * rollback refuses if anything changed after setup;
//!   * running setup twice is a no-op rather than a duplicate insert.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::non_empty_env;
use crate::model::Tone;
use crate::private_fs;
use crate::Result;

const SECTION: &str = "[ui.sidebar.spaces]";
const BACKUP_SUFFIX: &str = ".pulse-backup";
const META_SUFFIX: &str = ".meta";
const TRANSACTION_VERSION: u32 = 1;

/// Rows written into the user's config: red for an agent waiting on a human,
/// green for one working, grey for a quiet workspace. Colours chosen to read on
/// both light and dark themes.
///
/// Unlike collide, **all three** tones get a row, including the quiet one. A
/// quiet workspace with a sparkline showing it was busy ten minutes ago is not
/// nothing to display — it is the single most useful thing this plugin says. The
/// badge is only cleared for a workspace with no recorded history at all.
const TOKEN_COLOURS: [(&str, &str); 3] = [
    ("pulse_blocked", "#FF8080"),
    ("pulse_working", "#8CD98C"),
    ("pulse_quiet", "#8A8A8A"),
];

pub fn config_path() -> PathBuf {
    if let Some(explicit) = non_empty_env("HERDR_CONFIG_PATH") {
        return PathBuf::from(explicit);
    }
    herdr_dir().join("config.toml")
}

fn herdr_dir() -> PathBuf {
    if let Some(xdg) = non_empty_env("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("herdr");
    }
    match non_empty_env("HOME") {
        Some(home) => PathBuf::from(home).join(".config").join("herdr"),
        None => PathBuf::from(".config/herdr"),
    }
}

fn backup_path(config: &Path) -> PathBuf {
    let mut name = config.as_os_str().to_os_string();
    name.push(BACKUP_SUFFIX);
    PathBuf::from(name)
}

fn metadata_path(backup: &Path) -> PathBuf {
    let mut name = backup.as_os_str().to_os_string();
    name.push(META_SUFFIX);
    PathBuf::from(name)
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SetupTransaction {
    version: u32,
    original: String,
    installed: String,
    #[serde(default)]
    reloaded: bool,
}

/// The rows this plugin contributes, rendered as TOML lines at the indentation
/// herdr's own examples use.
fn token_lines() -> Vec<String> {
    TOKEN_COLOURS
        .iter()
        .map(|(token, colour)| format!("    {{ token = \"${token}\", fg = \"{colour}\" }},"))
        .collect()
}

fn already_configured(text: &str) -> bool {
    Tone::ALL_TOKENS
        .iter()
        .any(|token| text.contains(&format!("\"${token}\"")))
}

/// Splices the token entries into an existing `[ui.sidebar.spaces]` rows array,
/// or appends a complete section when the user has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// The file already names our tokens. A second run is a no-op rather than a
    /// duplicate insert.
    AlreadyConfigured,
    /// The updated file.
    Edit(String),
    /// `[ui.sidebar.spaces]` is present but there is nowhere safe to splice into.
    ///
    /// Distinguished from [`Plan::AlreadyConfigured`] because conflating the two
    /// is how `--setup` came to print "already configured; nothing to do",
    /// exit 0, and leave a file that named none of our tokens — after which the
    /// badge silently never appears and the user has been told everything is
    /// fine. A refusal has to look like a refusal.
    NoRowToEdit,
}

/// Plans the edit. See [`Plan`] for the three outcomes.
pub fn plan_edit(text: &str) -> Plan {
    if already_configured(text) {
        return Plan::AlreadyConfigured;
    }

    let lines: Vec<&str> = text.lines().collect();
    let section_start = lines
        .iter()
        .position(|line| line.trim_start().starts_with(SECTION));

    let Some(section_start) = section_start else {
        return Plan::Edit(append_section(text));
    };

    // Find the LAST row inside this section's rows array, and its final line.
    //
    // The entries have to land inside a row, not beside one. `rows` is an array
    // of arrays, and each inner array is one rendered line; a bare table dropped
    // between two rows is still valid TOML, so herdr accepts the file and then
    // renders nothing at all. That failure is invisible — which is exactly how it
    // shipped past a passing test suite once already.
    //
    // Depth 1 is inside `rows`, depth 2 is inside a row.
    let mut depth = 0usize;
    let mut in_rows = false;
    // (first line of the last row, last line of it, byte offset of the `]` that
    // closes it). The byte offset is carried rather than re-derived: on a rows
    // array written entirely on one line, `rfind(']')` finds the bracket closing
    // `rows` itself, and splicing there would put our entries *beside* the last
    // row instead of inside it — valid TOML that herdr accepts and then renders
    // nothing from, which is the exact invisible failure this function's comment
    // warns about.
    let mut row_span: Option<(usize, usize, usize)> = None;
    let mut row_start: Option<usize> = None;

    for (offset, line) in lines.iter().enumerate().skip(section_start + 1) {
        let trimmed = line.trim_start();
        // Where on this line the bracket scan should begin. For the `rows = [`
        // line itself the scan starts at its first `[`; for every later line it
        // starts at the beginning.
        let scan_from = if in_rows {
            0
        } else {
            if trimmed.starts_with('[') && !trimmed.starts_with("[[") {
                break; // next table; this section has no rows array
            }
            let Some(open) = line.find('[').filter(|_| trimmed.starts_with("rows")) else {
                continue;
            };
            in_rows = true;
            open
        };

        // Scanned character by character *including* the `rows = [` line, rather
        // than counting brackets on it and skipping ahead. Counting was wrong for
        // a rows array written on one line: `rows = [["a"],["b"]]` has three
        // opens and three closes, so the count came to zero, the line was
        // skipped, and no row was ever found — leaving a perfectly ordinary
        // config shape silently unconfigurable.
        for (column, ch) in line[scan_from..].char_indices() {
            match ch {
                '[' => {
                    depth += 1;
                    if depth == 2 && row_start.is_none() {
                        row_start = Some(offset);
                    }
                }
                ']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 1 {
                        if let Some(start) = row_start.take() {
                            row_span = Some((start, offset, scan_from + column));
                        }
                    }
                }
                _ => {}
            }
        }
        if depth == 0 {
            break; // rows array closed
        }
    }

    let Some((row_start, row_end, close)) = row_span else {
        return Plan::NoRowToEdit;
    };
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();

    if row_start == row_end {
        // The last row begins and ends on one line: splice the entries in just
        // before the bracket that closes *that row*, whose position the scan
        // recorded.
        let line = out[row_end].clone();
        let (head, tail) = line.split_at(close);
        let head = head.trim_end();
        let separator = if head.ends_with('[') { "" } else { "," };
        let entries: Vec<String> = token_lines()
            .into_iter()
            .map(|l| l.trim().trim_end_matches(',').to_string())
            .collect();
        out[row_end] = format!("{head}{separator} {}{tail}", entries.join(", "));
    } else {
        // A multi-line row: insert before its final line, which carries the
        // closing bracket. The preceding line already ends in a comma.
        for (n, line) in token_lines().into_iter().enumerate() {
            out.insert(row_end + n, line);
        }
    }
    Plan::Edit(finish(out, text))
}

/// The rows a user has to add by hand when the splice cannot find a safe place.
///
/// Shown as part of the error rather than pointing at the README: someone whose
/// config we just refused to edit should not have to go and look the snippet up.
fn manual_snippet() -> String {
    let mut out = String::from("  [\"branch\",\n");
    for line in token_lines() {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("  ],\n");
    out
}

fn append_section(text: &str) -> String {
    let mut out = text.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(SECTION);
    out.push_str("\nrows = [\n  [\"state_icon\", \"workspace\"],\n  [\"branch\",\n");
    for line in token_lines() {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("  ],\n]\n");
    out
}

fn finish(lines: Vec<String>, original: &str) -> String {
    let mut out = lines.join("\n");
    if original.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub fn run_setup() -> Result<()> {
    let config = config_path();
    let target = fs::canonicalize(&config)
        .map_err(|err| format!("cannot resolve {}: {err}", config.display()))?;
    let original = fs::read_to_string(&target)
        .map_err(|err| format!("cannot read {}: {err}", config.display()))?;
    let backup = backup_path(&config);
    let metadata = metadata_path(&backup);
    let installed = match plan_edit(&original) {
        Plan::AlreadyConfigured => {
            if !backup.exists() && !metadata.exists() {
                println!("pulse: sidebar tokens are already configured; nothing to do.");
                return Ok(());
            }
            if !backup.exists() {
                return Err(format!(
                    "setup metadata exists at {}, but its backup {} is missing; refusing to guess",
                    metadata.display(),
                    backup.display()
                )
                .into());
            }
            let saved_original = private_fs::read_to_string(&backup)?;
            let transaction = load_transaction(&backup, &metadata, &saved_original)?;
            if transaction.installed != original {
                return Err(format!(
                    "setup recovery files exist, but {} no longer matches their installed \
                     config; use `pulse --setup-rollback` for non-destructive recovery",
                    config.display()
                )
                .into());
            }
            if transaction.reloaded {
                println!("pulse: sidebar tokens are already configured; nothing to do.");
                return Ok(());
            }
            return finish_setup_reload(
                &config,
                &target,
                &saved_original,
                &backup,
                &metadata,
                true,
            );
        }
        Plan::NoRowToEdit => {
            return Err(format!(
                "{} has a {SECTION} section but no row this could be added to safely.\n\
                 Add the three entries by hand, inside one of the rows:\n\n{}",
                config.display(),
                manual_snippet()
            )
            .into());
        }
        Plan::Edit(updated) => updated,
    };

    if backup.exists() || metadata.exists() {
        return Err(format!(
            "refusing to overwrite setup recovery files at {} and {}; recover or move them first",
            backup.display(),
            metadata.display()
        )
        .into());
    }

    atomic_create_new(&backup, original.as_bytes(), 0o600)?;
    let transaction = SetupTransaction {
        version: TRANSACTION_VERSION,
        original: original.clone(),
        installed: installed.clone(),
        reloaded: false,
    };
    let encoded = serde_json::to_vec(&transaction)?;
    if let Err(meta_error) = atomic_create_new(&metadata, &encoded, 0o600) {
        return match remove_if_exists(&backup) {
            Ok(()) => Err(meta_error),
            Err(cleanup_error) => Err(format!(
                "could not record setup transaction ({meta_error}); recovery backup remains at \
                 {} because cleanup also failed: {cleanup_error}",
                backup.display()
            )
            .into()),
        };
    }
    if let Err(edit_error) = atomic_replace(&target, installed.as_bytes()) {
        return Err(format!(
            "could not install sidebar tokens ({edit_error}); recovery files remain at {} and {}",
            backup.display(),
            metadata.display()
        )
        .into());
    }

    finish_setup_reload(&config, &target, &original, &backup, &metadata, false)
}

fn finish_setup_reload(
    config: &Path,
    target: &Path,
    original: &str,
    backup: &Path,
    metadata: &Path,
    pending: bool,
) -> Result<()> {
    match reload_herdr_config() {
        Ok(()) => {
            mark_transaction_reloaded(metadata)?;
            if pending {
                println!(
                    "pulse: completed the pending sidebar reload for {}.",
                    config.display()
                );
            } else {
                println!(
                    "pulse: added sidebar tokens to {} (backup at {}).",
                    config.display(),
                    backup.display()
                );
            }
            println!("pulse: run `pulse --setup-rollback` to undo.");
            Ok(())
        }
        Err(ReloadError::Rejected { status, diagnostic }) => {
            if pending {
                return Err(format!(
                    "herdr still rejects the pending sidebar reload ({status}): {diagnostic}; \
                     the installed config and recovery files remain unchanged"
                )
                .into());
            }
            atomic_replace(target, original.as_bytes()).map_err(|restore_error| {
                format!(
                    "herdr rejected the updated config ({status}: {diagnostic}), and restoring {} \
                     also failed: {restore_error}; recovery files remain at {} and {}",
                    config.display(),
                    backup.display(),
                    metadata.display()
                )
            })?;
            remove_transaction(backup, metadata).map_err(|cleanup_error| {
                format!(
                    "herdr rejected the updated config ({status}: {diagnostic}); the config was \
                     restored, but recovery cleanup failed: {cleanup_error}"
                )
            })?;
            Err(format!(
                "herdr rejected the updated config, so it was restored unchanged: {diagnostic}"
            )
            .into())
        }
        Err(error @ ReloadError::Launch { .. }) => Err(format!(
            "{error}. The complete sidebar edit remains in {}; fix HERDR_BIN_PATH, run {}, then \
             keep {} and {} until reload succeeds or rollback completes",
            config.display(),
            error.reload_instruction(),
            backup.display(),
            metadata.display()
        )
        .into()),
    }
}

pub fn run_rollback() -> Result<()> {
    let config = config_path();
    let target = fs::canonicalize(&config)
        .map_err(|err| format!("cannot resolve {}: {err}", config.display()))?;
    let backup = backup_path(&config);
    let metadata = metadata_path(&backup);
    if !backup.exists() {
        return Err(format!("no backup found at {}", backup.display()).into());
    }

    let current = fs::read_to_string(&target)?;
    let original = private_fs::read_to_string(&backup)?;
    let installed = load_transaction(&backup, &metadata, &original)?.installed;
    if current == original {
        remove_transaction(&backup, &metadata)?;
        println!(
            "pulse: {} was already restored; setup recovery files removed.",
            config.display()
        );
        return Ok(());
    }
    if current != installed {
        return Err(format!(
            "refusing to overwrite {} because it changed after pulse setup. Remove only \
             `$pulse_blocked`, `$pulse_working`, and `$pulse_quiet` entries from one \
             `[ui.sidebar.spaces]` row, run `herdr server reload-config`, verify the sidebar, \
             then delete {} and {}. Do not copy the backup over the current file if you need \
             to preserve later edits.",
            config.display(),
            backup.display(),
            metadata.display()
        )
        .into());
    }

    atomic_replace(&target, original.as_bytes())?;
    match reload_herdr_config() {
        Ok(()) => {
            remove_transaction(&backup, &metadata)?;
            println!("pulse: restored {} from backup.", config.display());
            Ok(())
        }
        Err(ReloadError::Rejected { status, diagnostic }) => {
            atomic_replace(&target, installed.as_bytes()).map_err(|restore_error| {
                format!(
                    "herdr rejected the rollback ({status}: {diagnostic}), and restoring the \
                     installed config also failed: {restore_error}; recovery files remain"
                )
            })?;
            Err(format!(
                "herdr rejected the rollback, so the installed config was restored: {diagnostic}"
            )
            .into())
        }
        Err(error @ ReloadError::Launch { .. }) => Err(format!(
            "{error}. The requested rollback is complete on disk at {}; run {}, then run \
             `pulse --setup-rollback` again to remove {} and {}",
            config.display(),
            error.reload_instruction(),
            backup.display(),
            metadata.display()
        )
        .into()),
    }
}

fn load_transaction(backup: &Path, metadata: &Path, original: &str) -> Result<SetupTransaction> {
    if metadata.exists() {
        let raw = private_fs::read(metadata)?;
        let transaction: SetupTransaction = serde_json::from_slice(&raw)
            .map_err(|err| format!("{} is not valid setup metadata: {err}", metadata.display()))?;
        if transaction.version != TRANSACTION_VERSION {
            return Err(format!(
                "{} is transaction version {}, expected {TRANSACTION_VERSION}",
                metadata.display(),
                transaction.version
            )
            .into());
        }
        if transaction.original != original {
            return Err(format!(
                "{} does not match the immutable backup at {}; refusing rollback",
                metadata.display(),
                backup.display()
            )
            .into());
        }
        return Ok(transaction);
    }

    match plan_edit(original) {
        Plan::Edit(installed) => Ok(SetupTransaction {
            version: TRANSACTION_VERSION,
            original: original.to_string(),
            installed,
            // Legacy backups were retained only after a successful reload.
            reloaded: true,
        }),
        _ => Err(format!(
            "legacy backup at {} cannot be mapped to one exact installed config; refusing rollback",
            backup.display()
        )
        .into()),
    }
}

fn mark_transaction_reloaded(metadata: &Path) -> Result<()> {
    if !metadata.exists() {
        return Ok(());
    }
    let mut transaction: SetupTransaction = serde_json::from_slice(&private_fs::read(metadata)?)
        .map_err(|err| format!("{} is not valid setup metadata: {err}", metadata.display()))?;
    transaction.reloaded = true;
    atomic_replace(metadata, &serde_json::to_vec(&transaction)?)
}

fn remove_transaction(backup: &Path, metadata: &Path) -> Result<()> {
    remove_if_exists(metadata)?;
    remove_if_exists(backup)?;
    Ok(())
}

fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[derive(Debug)]
enum ReloadError {
    Launch {
        binary: String,
        source: std::io::Error,
    },
    Rejected {
        status: ExitStatus,
        diagnostic: String,
    },
}

impl ReloadError {
    fn reload_instruction(&self) -> String {
        let binary = match self {
            Self::Launch { binary, .. } => binary,
            Self::Rejected { .. } => "herdr",
        };
        format!("`{binary} server reload-config`")
    }
}

impl std::fmt::Display for ReloadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Launch { binary, source } => {
                write!(
                    formatter,
                    "could not launch `{binary}` to reload herdr config: {source}"
                )
            }
            Self::Rejected { status, diagnostic } => {
                write!(formatter, "herdr reload failed with {status}: {diagnostic}")
            }
        }
    }
}

impl std::error::Error for ReloadError {}

/// Sidebar rows reload live, so the user never has to restart herdr.
fn reload_herdr_config() -> std::result::Result<(), ReloadError> {
    let bin = non_empty_env("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".to_string());
    let binary = bin.clone();
    let output = std::process::Command::new(bin)
        .args(["server", "reload-config"])
        .output()
        .map_err(|source| ReloadError::Launch { binary, source })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let diagnostic = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("process exited with {}", output.status)
    };
    Err(ReloadError::Rejected {
        status: output.status,
        diagnostic,
    })
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let mode = fs::metadata(path)?.permissions().mode();
    let temp = stage(path, bytes, mode)?;
    if let Err(err) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(err.into());
    }
    sync_parent(path)?;
    Ok(())
}

fn atomic_create_new(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let temp = stage(path, bytes, mode)?;
    if let Err(err) = fs::hard_link(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(err.into());
    }
    fs::remove_file(&temp)?;
    sync_parent(path)?;
    Ok(())
}

fn stage(path: &Path, bytes: &[u8], mode: u32) -> Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)?;
    let mut name = path
        .file_name()
        .ok_or_else(|| format!("{} has no file name", path.display()))?
        .to_os_string();
    name.push(format!(
        ".pulse-tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let temp = parent.join(name);
    let write_result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(err) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(err.into());
    }
    Ok(temp)
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
