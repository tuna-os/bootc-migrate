use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct OstreeFileObject {
    pub path: PathBuf,
    #[allow(dead_code)]
    pub ostree_checksum: String,
}

pub fn scan_ostree_file_objects<P: AsRef<Path>>(repo_path: P) -> Result<Vec<OstreeFileObject>> {
    let objects_dir = repo_path.as_ref().join("objects");
    let mut file_objects = Vec::new();

    if !objects_dir.exists() {
        return Ok(file_objects);
    }

    // Walk objects/xx/ directories
    for entry in fs::read_dir(&objects_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // Check if directory name is a 2-char hex string
            if dir_name.len() == 2 && dir_name.chars().all(|c| c.is_ascii_hexdigit()) {
                for file_entry in fs::read_dir(&path)? {
                    let file_entry = file_entry?;
                    let file_path = file_entry.path();
                    if file_path.is_file() {
                        let file_name =
                            file_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        // Check if it ends with .file
                        if let Some(remaining_hex) = file_name.strip_suffix(".file") {
                            // remove ".file"
                            let ostree_checksum = format!("{}{}", dir_name, remaining_hex);
                            file_objects.push(OstreeFileObject {
                                path: file_path,
                                ostree_checksum,
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(file_objects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn test_scan_ostree_objects() {
        let dir = tempdir().unwrap();
        let objects_dir = dir.path().join("objects");
        let sub_dir = objects_dir.join("ab");
        fs::create_dir_all(&sub_dir).unwrap();

        let file_path = sub_dir.join("cdef1234.file");
        let mut file = fs::File::create(&file_path).unwrap();
        file.write_all(b"test file content").unwrap();
        drop(file);

        let list = scan_ostree_file_objects(dir.path()).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].ostree_checksum, "abcdef1234");
        assert_eq!(list[0].path, file_path);
    }
}
