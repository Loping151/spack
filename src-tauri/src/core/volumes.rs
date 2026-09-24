use super::container::{self, tick, Cancel, ProgressFn};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};
const MAGIC: &[u8; 8] = b"SPVOL\0\x02\0";
pub const HEADER: u64 = 96;
const MAX_PARTS: u32 = 10_000;
#[derive(Clone, Debug)]
pub enum Split {
    None,
    Count(u32),
    Size(u64),
}
impl Split {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Count(n) if *n < 2 || *n > MAX_PARTS => Err("error.part_count_range".into()),
            Self::Size(n) if *n <= HEADER => Err("error.part_size_min".into()),
            _ => Ok(()),
        }
    }
}
fn io(e: std::io::Error) -> String {
    e.to_string()
}
#[derive(Clone)]
struct Header {
    index: u32,
    count: u32,
    total: u64,
    len: u64,
    archive: [u8; 32],
    hash: [u8; 32],
}
impl Header {
    fn read(f: &mut File) -> Result<Self, String> {
        let mut b = [0; 96];
        f.read_exact(&mut b).map_err(io)?;
        if &b[..8] != MAGIC {
            return Err("error.not_volume".into());
        }
        let h = Self {
            index: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            count: u32::from_le_bytes(b[12..16].try_into().unwrap()),
            total: u64::from_le_bytes(b[16..24].try_into().unwrap()),
            len: u64::from_le_bytes(b[24..32].try_into().unwrap()),
            archive: b[32..64].try_into().unwrap(),
            hash: b[64..96].try_into().unwrap(),
        };
        if h.count < 2
            || h.count > MAX_PARTS
            || h.index == 0
            || h.index > h.count
            || h.len == 0
            || h.total < h.len
            || f.metadata().map_err(io)?.len()
                != h.len
                    .checked_add(HEADER)
                    .ok_or("error.part_length_overflow")?
        {
            return Err("error.invalid_volume_header".into());
        }
        Ok(h)
    }
    fn write(&self, f: &mut File) -> Result<(), String> {
        f.write_all(MAGIC).map_err(io)?;
        f.write_all(&self.index.to_le_bytes()).map_err(io)?;
        f.write_all(&self.count.to_le_bytes()).map_err(io)?;
        f.write_all(&self.total.to_le_bytes()).map_err(io)?;
        f.write_all(&self.len.to_le_bytes()).map_err(io)?;
        f.write_all(&self.archive).map_err(io)?;
        f.write_all(&self.hash).map_err(io)
    }
}
fn part_path(base: &Path, index: u32) -> PathBuf {
    let mut s = base.as_os_str().to_os_string();
    s.push(format!(".{index:03}"));
    PathBuf::from(s)
}
fn base_from_part(path: &Path, h: &Header) -> Result<PathBuf, String> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .ok_or("error.missing_part_number")?;
    if ext.parse::<u32>().ok() != Some(h.index) {
        return Err("error.part_name_mismatch".into());
    }
    let base = path.with_extension("");
    if !base
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("spk") || extension.eq_ignore_ascii_case("spack")
        })
    {
        return Err("error.invalid_volume_extension".into());
    }
    Ok(base)
}
pub fn part_count(path: &Path) -> Result<usize, String> {
    let mut f = File::open(path).map_err(io)?;
    let mut b = [0; 8];
    f.read_exact(&mut b).map_err(io)?;
    if &b == MAGIC {
        use std::io::{Seek, SeekFrom};
        f.seek(SeekFrom::Start(0)).map_err(io)?;
        Ok(Header::read(&mut f)?.count as usize)
    } else if &b == container::MAGIC {
        Ok(1)
    } else {
        Err("error.unsupported_archive".into())
    }
}

pub fn publish(
    archive: &Path,
    dest: &Path,
    split: &Split,
    cb: ProgressFn,
    cancel: &Cancel,
) -> Result<Vec<PathBuf>, String> {
    split.validate()?;
    let total = fs::metadata(archive).map_err(io)?.len();
    if let Split::Size(size) = split {
        if total <= *size {
            return publish(archive, dest, &Split::None, cb, cancel);
        }
    }
    if matches!(split, Split::None) {
        let mut staged =
            tempfile::NamedTempFile::new_in(dest.parent().unwrap_or(Path::new("."))).map_err(io)?;
        let mut src = File::open(archive).map_err(io)?;
        let mut buf = vec![0; 1 << 20];
        let mut done = 0;
        loop {
            tick(cb, cancel, "split", done, total, "")?;
            let n = src.read(&mut buf).map_err(io)?;
            if n == 0 {
                break;
            }
            staged.write_all(&buf[..n]).map_err(io)?;
            done += n as u64;
        }
        staged.as_file().sync_all().map_err(io)?;
        tick(cb, cancel, "split", total, total, "")?;
        staged
            .persist_noclobber(dest)
            .map_err(|e| crate::locale::message("error.save_failed", &[(e).to_string()]))?;
        return Ok(vec![dest.to_path_buf()]);
    }
    let count = match split {
        Split::Count(n) => *n,
        Split::Size(size) => u32::try_from(total.div_ceil(size - HEADER))
            .map_err(|_| "error.too_many_parts")?
            .max(2),
        _ => unreachable!(),
    };
    if count > MAX_PARTS || total < count as u64 {
        return Err("error.part_count_exceeds_data".into());
    }
    let paths: Vec<_> = (1..=count).map(|i| part_path(dest, i)).collect();
    for path in &paths {
        if path.exists() {
            return Err(crate::locale::message(
                "error.file_exists",
                &[(path.display()).to_string()],
            ));
        }
    }
    let archive_hash = container::hash_file(archive)?;
    let mut ah = [0; 32];
    for i in 0..32 {
        ah[i] =
            u8::from_str_radix(&archive_hash[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string())?;
    }
    let mut source = File::open(archive).map_err(io)?;
    let mut done = 0;
    let mut saved: Vec<PathBuf> = Vec::new();
    let mut buf = vec![0; 1 << 20];
    let result = (|| {
        for (i, path) in paths.iter().enumerate() {
            let len = match split {
                Split::Count(_) => {
                    total / count as u64 + u64::from((i as u64) < total % count as u64)
                }
                Split::Size(size) => (total - done).min(size - HEADER),
                _ => unreachable!(),
            };
            if len == 0 {
                break;
            }
            let mut staged = tempfile::NamedTempFile::new_in(path.parent().unwrap()).map_err(io)?;
            let mut h = Header {
                index: i as u32 + 1,
                count,
                total,
                len,
                archive: ah,
                hash: [0; 32],
            };
            h.write(staged.as_file_mut())?;
            let mut hash = blake3::Hasher::new();
            let mut remaining = len;
            while remaining > 0 {
                tick(
                    cb,
                    cancel,
                    "split",
                    done,
                    total,
                    format!("{} / {count}", i + 1),
                )?;
                let n = remaining.min(buf.len() as u64) as usize;
                source.read_exact(&mut buf[..n]).map_err(io)?;
                staged.write_all(&buf[..n]).map_err(io)?;
                hash.update(&buf[..n]);
                remaining -= n as u64;
                done += n as u64;
            }
            h.hash = *hash.finalize().as_bytes();
            use std::io::{Seek, SeekFrom};
            staged.seek(SeekFrom::Start(0)).map_err(io)?;
            h.write(staged.as_file_mut())?;
            staged.as_file().sync_all().map_err(io)?;
            staged.persist_noclobber(path).map_err(|e| e.to_string())?;
            saved.push(path.clone());
        }
        if saved.len() != count as usize {
            return Err("error.empty_part".into());
        }
        tick(cb, cancel, "split", total, total, "")?;
        Ok(saved.clone())
    })();
    if result.is_err() {
        for p in saved {
            let _ = fs::remove_file(p);
        }
    }
    result
}

pub enum Resolved {
    Original(PathBuf),
    Joined(tempfile::NamedTempFile),
}
impl Resolved {
    pub fn path(&self) -> &Path {
        match self {
            Self::Original(p) => p,
            Self::Joined(f) => f.path(),
        }
    }
}
pub fn resolve(path: &Path, cb: ProgressFn, cancel: &Cancel) -> Result<Resolved, String> {
    let mut file = File::open(path).map_err(io)?;
    let mut magic = [0; 8];
    file.read_exact(&mut magic).map_err(io)?;
    if &magic == container::MAGIC {
        return Ok(Resolved::Original(path.to_path_buf()));
    }
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0)).map_err(io)?;
    let initial = Header::read(&mut file)?;
    let base = base_from_part(path, &initial)?;
    let mut sum = 0u64;
    for index in 1..=initial.count {
        tick(
            cb,
            cancel,
            "verify",
            index as u64 - 1,
            initial.count as u64,
            index.to_string(),
        )?;
        let p = part_path(&base, index);
        let mut f = File::open(&p).map_err(|e| {
            crate::locale::message(
                "error.volume_read",
                &[(p.display()).to_string(), (e).to_string()],
            )
        })?;
        let h = Header::read(&mut f)?;
        if h.index != index
            || h.count != initial.count
            || h.total != initial.total
            || h.archive != initial.archive
        {
            return Err(crate::locale::message(
                "error.volume_set_at",
                &[(p.display()).to_string()],
            ));
        }
        sum = sum.checked_add(h.len).ok_or("error.part_length_overflow")?;
    }
    if sum != initial.total {
        return Err("error.parts_length_mismatch".into());
    }
    let mut joined = tempfile::Builder::new()
        .prefix("spack-join-")
        .tempfile()
        .map_err(io)?;
    let mut whole = blake3::Hasher::new();
    let mut done = 0;
    let mut buf = vec![0; 1 << 20];
    for index in 1..=initial.count {
        let mut f = File::open(part_path(&base, index)).map_err(io)?;
        let h = Header::read(&mut f)?;
        if h.index != index
            || h.count != initial.count
            || h.total != initial.total
            || h.archive != initial.archive
        {
            return Err("error.volume_changed".into());
        }
        let mut hash = blake3::Hasher::new();
        let mut remaining = h.len;
        while remaining > 0 {
            tick(
                cb,
                cancel,
                "verify",
                done,
                sum,
                format!("{index} / {}", initial.count),
            )?;
            let n = remaining.min(buf.len() as u64) as usize;
            f.read_exact(&mut buf[..n]).map_err(io)?;
            joined.write_all(&buf[..n]).map_err(io)?;
            hash.update(&buf[..n]);
            whole.update(&buf[..n]);
            done += n as u64;
            remaining -= n as u64;
        }
        if hash.finalize().as_bytes() != &h.hash {
            return Err(crate::locale::message(
                "error.volume_checksum",
                &[(index).to_string()],
            ));
        }
    }
    if whole.finalize().as_bytes() != &initial.archive {
        return Err("error.archive_checksum".into());
    }
    joined.flush().map_err(io)?;
    Ok(Resolved::Joined(joined))
}

pub fn preview_reader(path: &Path) -> Result<Box<dyn Read>, String> {
    use std::io::{Seek, SeekFrom};
    let mut file = File::open(path).map_err(io)?;
    let mut magic = [0; 8];
    file.read_exact(&mut magic).map_err(io)?;
    file.seek(SeekFrom::Start(0)).map_err(io)?;
    if &magic == container::MAGIC {
        return Ok(Box::new(file));
    }
    let initial = Header::read(&mut file)?;
    let base = base_from_part(path, &initial)?;
    Ok(Box::new(PrefixReader {
        base,
        initial,
        index: 0,
        remaining: 0,
        current: None,
    }))
}
struct PrefixReader {
    base: PathBuf,
    initial: Header,
    index: u32,
    remaining: u64,
    current: Option<File>,
}
impl Read for PrefixReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            if self.index >= self.initial.count {
                return Ok(0);
            }
            self.index += 1;
            let path = part_path(&self.base, self.index);
            let mut f = File::open(&path)?;
            let h = Header::read(&mut f).map_err(std::io::Error::other)?;
            if h.index != self.index
                || h.count != self.initial.count
                || h.total != self.initial.total
                || h.archive != self.initial.archive
            {
                return Err(std::io::Error::other("error.volume_set_mismatch"));
            }
            self.remaining = h.len;
            self.current = Some(f);
        }
        let take = buf.len().min(self.remaining as usize);
        let n = self.current.as_mut().unwrap().read(&mut buf[..take])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "error.volume_truncated",
            ));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}
