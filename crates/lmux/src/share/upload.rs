//! Files sent from the phone over the share's WebSocket.
//!
//! The page or app announces one file with `{"t":"upload","id","name","size"}`,
//! then sends its bytes as binary frames in order. When the last byte arrives
//! the file is written into this share's private folder and the server answers
//! `{"t":"uploaded","id","path"}`; the viewer then types that path into the
//! agent like any other text. Failures answer `{"t":"upload_error","id","msg"}`.
//!
//! Uploads are input to the Mac, so they need a share with typing allowed.
//! File contents and names are never logged.

use std::path::{Path, PathBuf};

/// The largest file one upload may carry.
pub const MAX_UPLOAD: u64 = 25 * 1024 * 1024;

/// One upload in progress on one socket.
pub struct Pending {
    pub id: String,
    name: String,
    size: u64,
    data: Vec<u8>,
}

pub enum Step {
    /// More bytes are expected.
    More,
    /// Every byte arrived.
    Done,
    /// More bytes arrived than were announced.
    TooMany,
}

impl Pending {
    /// Checks an `upload` announcement.
    pub fn start(id: &str, name: &str, size: u64) -> Result<Pending, &'static str> {
        if id.is_empty() || id.len() > 64 {
            return Err("Bad upload id.");
        }
        if size == 0 {
            return Err("The file is empty.");
        }
        if size > MAX_UPLOAD {
            return Err("The file is too large (25 MB at most).");
        }
        Ok(Pending { id: id.to_owned(), name: safe_name(name), size, data: Vec::with_capacity(size as usize) })
    }

    /// A complete upload, as a gateway hands it to the server that owns the tab.
    pub fn whole(name: &str, data: Vec<u8>) -> Result<Pending, &'static str> {
        let mut p = Pending::start("remote", name, data.len() as u64)?;
        p.data = data;
        Ok(p)
    }

    /// The safe name and the bytes, for sending on to another server.
    pub fn into_parts(self) -> (String, Vec<u8>) {
        (self.name, self.data)
    }

    pub fn push(&mut self, bytes: &[u8]) -> Step {
        if self.data.len() as u64 + bytes.len() as u64 > self.size {
            return Step::TooMany;
        }
        self.data.extend_from_slice(bytes);
        if self.data.len() as u64 == self.size { Step::Done } else { Step::More }
    }

    /// Writes the file into `dir` (created private on first use) under a name
    /// no other upload has, and returns its path.
    pub fn save(self, dir: &Path) -> std::io::Result<PathBuf> {
        create_private_dir(dir)?;
        let (stem, ext) = match self.name.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() && !e.is_empty() => (s.to_owned(), format!(".{e}")),
            _ => (self.name.clone(), String::new()),
        };
        for n in 0..1000u32 {
            let file = if n == 0 { format!("{stem}{ext}") } else { format!("{stem}-{n}{ext}") };
            let path = dir.join(file);
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    std::io::Write::write_all(&mut f, &self.data)?;
                    return Ok(path);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other("too many files with this name"))
    }
}

/// The folder for one share's uploads: `$TMPDIR/lmux-share-<random>`.
pub fn folder(token: &str) -> PathBuf {
    std::env::temp_dir().join(format!("lmux-share-{}", &token[..token.len().min(12)]))
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
}

/// The last path part, reduced to letters, digits, `.`, `-` and `_`, so the
/// path is safe to type into a shell or an agent prompt as is.
pub fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut out: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '-' })
        .collect();
    while out.starts_with('.') || out.starts_with('-') {
        out.remove(0);
    }
    out.truncate(80);
    if out.trim_matches(['.', '-', '_']).is_empty() { "file".to_owned() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_made_safe() {
        assert_eq!(safe_name("photo.jpg"), "photo.jpg");
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("C:\\x\\My File (1).PNG"), "My-File--1-.PNG");
        assert_eq!(safe_name(".bashrc"), "bashrc");
        assert_eq!(safe_name("$(rm -rf ~).txt"), "rm--rf---.txt");
        assert_eq!(safe_name(""), "file");
        assert_eq!(safe_name("..."), "file");
        assert_eq!(safe_name(&"a".repeat(200)).len(), 80);
    }

    #[test]
    fn sizes_are_checked() {
        assert!(Pending::start("1", "a", 0).is_err());
        assert!(Pending::start("1", "a", MAX_UPLOAD + 1).is_err());
        assert!(Pending::start("", "a", 3).is_err());
        let mut p = Pending::start("1", "a", 3).unwrap();
        assert!(matches!(p.push(b"ab"), Step::More));
        assert!(matches!(p.push(b"cd"), Step::TooMany));
        assert!(matches!(p.push(b"c"), Step::Done));
    }

    #[test]
    fn saves_without_overwriting() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("up");
        let mut a = Pending::start("1", "notes.txt", 2).unwrap();
        a.push(b"hi");
        let pa = a.save(&dir).unwrap();
        let mut b = Pending::start("2", "notes.txt", 3).unwrap();
        b.push(b"bye");
        let pb = b.save(&dir).unwrap();
        assert_eq!(pa.file_name().unwrap(), "notes.txt");
        assert_eq!(pb.file_name().unwrap(), "notes-1.txt");
        assert_eq!(std::fs::read(&pa).unwrap(), b"hi");
        assert_eq!(std::fs::read(&pb).unwrap(), b"bye");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }
}
