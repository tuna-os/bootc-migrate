pub mod grub;
pub mod live;
pub mod systemd_boot;

/// A BLS (Boot Loader Specification) Type 1 entry.
#[derive(Debug)]
pub struct BlsEntry {
    pub title: String,
    pub version: String,
    pub linux: String,
    pub initrds: Vec<String>,
    pub options: String,
    pub filename: String,
    pub sort_key: String,
}

impl BlsEntry {
    /// Render the entry as a BLS .conf file.
    pub fn render(&self) -> String {
        let mut out = format!(
            "title {}\nversion {}\nlinux {}\noptions {}\nsort-key {}\n",
            self.title, self.version, self.linux, self.options, self.sort_key
        );
        for i in &self.initrds {
            out.push_str(&format!("initrd {}\n", i));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bls_entry_renders_correctly() {
        let entry = BlsEntry {
            title: "Fedora (composefs)".into(),
            version: "6.8.0".into(),
            linux: "/bootc_composefs-abc/vmlinuz".into(),
            initrds: vec!["/bootc_composefs-abc/initrd".into()],
            options: "rw quiet composefs=abc123".into(),
            filename: "bootc_fedora-41-1.conf".into(),
            sort_key: "bootc-fedora-0".into(),
        };
        let rendered = entry.render();
        assert!(rendered.contains("title Fedora (composefs)"));
        assert!(rendered.contains("linux /bootc_composefs-abc/vmlinuz"));
        assert!(rendered.contains("composefs=abc123"));
        assert!(rendered.contains("sort-key bootc-fedora-0"));
    }
}
