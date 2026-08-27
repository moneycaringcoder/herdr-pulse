//! Unix-only ownership and mode enforcement for plugin-owned runtime state.

use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

pub const DIR_MODE: u32 = 0o700;
pub const FILE_MODE: u32 = 0o600;

pub fn ensure_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut builder = DirBuilder::new();
            builder.mode(DIR_MODE);
            if let Err(err) = builder.create(path) {
                if err.kind() != io::ErrorKind::AlreadyExists {
                    return Err(err);
                }
            }
        }
        Err(err) => return Err(err),
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = directory.metadata()?;
    validate_owner(path, &metadata)?;
    if !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "plugin state path {} is not a directory",
            path.display()
        )));
    }
    directory.set_permissions(Permissions::from_mode(DIR_MODE))
}

pub fn open(path: &Path) -> io::Result<File> {
    open_with(path, false, false)
}

pub fn create_new(path: &Path) -> io::Result<File> {
    open_with(path, true, false)
}

pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;

    let mut file = open_with(path, false, true)?;
    file.write_all(bytes)
}

pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read as _;

    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    tighten_file(path, &file)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub fn read_to_string(path: &Path) -> io::Result<String> {
    String::from_utf8(read(path)?).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn open_with(path: &Path, create_new: bool, truncate: bool) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .mode(FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW)
        .truncate(false);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true);
    }
    let file = options.open(path)?;
    tighten_file(path, &file)?;
    if truncate {
        file.set_len(0)?;
    }
    Ok(file)
}

fn tighten_file(path: &Path, file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    validate_owner(path, &metadata)?;
    if !metadata.is_file() {
        return Err(io::Error::other(format!(
            "plugin state path {} is not a regular file",
            path.display()
        )));
    }
    file.set_permissions(Permissions::from_mode(FILE_MODE))
}

fn validate_owner(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    let effective = unsafe { libc::geteuid() };
    if metadata.uid() != effective {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "plugin state path {} belongs to uid {}, expected {}",
                path.display(),
                metadata.uid(),
                effective
            ),
        ));
    }
    Ok(())
}
