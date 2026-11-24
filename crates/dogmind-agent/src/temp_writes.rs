//! What the app writes into a folder a program in the sandbox can write too (the sandbox's temp folder): the page a check opens, the CDN files it
//! keeps, the copies of attached pictures, the scratch folder. A program there could have made a folder on the way a junction, or a name a link, to a
//! place outside it, where the app (which is not in the sandbox) would then write. So a file's folder must be under that temp folder as the file
//! system has it, and a name already there that leads elsewhere is not written through.

use std::path::{Path, PathBuf};

/// Whether `metadata` is of a link or a junction: a reparse point on Windows, a symbolic link elsewhere.
fn leads_elsewhere(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // (FILE_ATTRIBUTE_REPARSE_POINT)
        metadata.file_attributes() & 0x400 != 0 || metadata.file_type().is_symlink()
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// The folder as the file system has it, links and junctions followed.
fn real(path: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(path).map_err(|error| format!("{}: {error}", path.display()))
}

/// Makes `file`'s folder, if it is not there, where it is under `temp` as the file system has it, and says whether `file` may be written: no folder on
/// its way leads outside `temp`, and `file` is not a link or a junction already. The folders on the way that are there are looked at before any is
/// made, so that none is made through a junction either.
pub fn place_for(temp: &Path, file: &Path) -> Result<(), String> {
    let real_temp = real(temp)?;
    let folder = file.parent().ok_or_else(|| format!("{}: no folder", file.display()))?;
    // (the nearest folder on the way that is there: what is made below it stays where it is)
    let mut there = folder;
    while !there.exists() {
        there = there.parent().ok_or_else(|| format!("{}: no folder there", folder.display()))?;
    }
    if !real(there)?.starts_with(&real_temp) {
        return Err(format!("{} leads outside {}", there.display(), temp.display()));
    }
    std::fs::create_dir_all(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    if !real(folder)?.starts_with(&real_temp) {
        return Err(format!("{} leads outside {}", folder.display(), temp.display()));
    }
    if std::fs::symlink_metadata(file).is_ok_and(|metadata| leads_elsewhere(&metadata)) {
        return Err(format!("{} is a link or a junction", file.display()));
    }
    Ok(())
}

/// `bytes` written to `file`, where [`place_for`] says it may be.
pub fn write(temp: &Path, file: &Path, bytes: impl AsRef<[u8]>) -> Result<(), String> {
    place_for(temp, file)?;
    std::fs::write(file, bytes).map_err(|error| format!("{}: {error}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_under_the_folder_is_written_and_its_folders_made() {
        let temp = crate::in_container::tempdir().unwrap();
        let file = temp.path().join("a").join("b").join("c.txt");
        write(temp.path(), &file, "here").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "here");
        assert!(write(temp.path(), &temp.path().join("..").join("outside.txt"), "x").is_err(), "a `..` climbs out");
    }

    /// A folder on the way made a junction to a place outside, as a program in the sandbox could make one: nothing is written there, nor any folder made.
    #[cfg(windows)]
    #[test]
    fn a_junction_on_the_way_is_not_written_through() {
        let temp = crate::in_container::tempdir().unwrap();
        let outside = crate::in_container::tempdir().unwrap();
        let link = temp.path().join("cdn");
        let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(&link).arg(outside.path()).output().unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stdout));
        assert!(write(temp.path(), &link.join("x.js"), "x").is_err());
        assert!(write(temp.path(), &link.join("deep").join("x.js"), "x").is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0, "nothing written or made outside");
        // A name that is a junction itself is not written through either.
        let named = temp.path().join("page.html");
        let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(&named).arg(outside.path()).output().unwrap();
        assert!(made.status.success());
        assert!(write(temp.path(), &named, "x").is_err());
    }
}
