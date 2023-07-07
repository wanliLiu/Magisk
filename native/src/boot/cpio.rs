use std::collections::BTreeMap;
use std::ffi::CStr;
use std::fmt::{Display, Formatter, Write as FmtWrite};
use std::fs::{metadata, read, DirBuilder, File};
use std::io::Write;
use std::mem::size_of;
use std::os::unix::fs::{symlink, DirBuilderExt, FileTypeExt, MetadataExt};
use std::path::Path;
use std::process::exit;
use std::slice;

use argh::{EarlyExit, FromArgs};
use size::{Base, Size, Style};

use base::libc::{
    c_char, dev_t, gid_t, major, makedev, minor, mknod, mode_t, uid_t, S_IFBLK, S_IFCHR, S_IFDIR,
    S_IFLNK, S_IFMT, S_IFREG, S_IRGRP, S_IROTH, S_IRUSR, S_IWGRP, S_IWOTH, S_IWUSR, S_IXGRP,
    S_IXOTH, S_IXUSR,
};
use base::{log_err, LoggedResult, MappedFile, ResultExt, StrErr, Utf8CStr, WriteExt};

use crate::ramdisk::MagiskCpio;

#[derive(FromArgs)]
#[argh(description = "Manipulate cpio archives; <command> --help for more info.")]
struct CpioCli {
    #[argh(subcommand)]
    command: CpioCommands,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum CpioCommands {
    Test(Test),
    Restore(Restore),
    Patch(Patch),
    Exists(Exists),
    Backup(Backup),
    Remove(Remove),
    Move(Move),
    Extract(Extract),
    MakeDir(MakeDir),
    Link(Link),
    Add(Add),
    List(List),
}

#[derive(FromArgs)]
#[argh(
    subcommand,
    name = "test",
    description = "Test the cpio's status; return value is 0 or bitwise or-ed of following values: 0x1:Magisk; 0x2:unsupported; 0x4:Sony"
)]
struct Test {}

#[derive(FromArgs)]
#[argh(
    subcommand,
    name = "restore",
    description = "Restore ramdisk from ramdisk backup stored within incpio"
)]
struct Restore {}

#[derive(FromArgs)]
#[argh(
    subcommand,
    name = "patch",
    description = "Apply ramdisk patches; configure with env variables: KEEPVERITY KEEPFORCEENCRYPT"
)]
struct Patch {}

#[derive(FromArgs)]
#[argh(
    subcommand,
    name = "exists",
    description = "Return 0 if <entry> exists, otherwise return 1"
)]
struct Exists {
    #[argh(positional, arg_name = "entry")]
    path: String,
}

#[derive(FromArgs)]
#[argh(
    subcommand,
    name = "backup",
    description = "Create ramdisk backups from <orig>"
)]
struct Backup {
    #[argh(positional, arg_name = "orig")]
    origin: String,
}

#[derive(FromArgs)]
#[argh(
    subcommand,
    name = "rm",
    description = "Remove <entry>; specify [-r] to remove recursively"
)]
struct Remove {
    #[argh(positional, arg_name = "entry")]
    path: String,
    #[argh(switch, short = 'r', description = "recursive")]
    recursive: bool,
}

#[derive(FromArgs)]
#[argh(subcommand, name = "mv", description = "Move <source> to <dest>")]
struct Move {
    #[argh(positional, arg_name = "source")]
    from: String,
    #[argh(positional, arg_name = "dest")]
    to: String,
}

#[derive(FromArgs)]
#[argh(
    subcommand,
    name = "extract",
    description = "Extract <paths[0]> to <paths[1]>, or extract all entries to current directory if <paths> is not given"
)]
struct Extract {
    #[argh(positional, greedy)]
    paths: Vec<String>,
}

#[derive(FromArgs)]
#[argh(
    subcommand,
    name = "mkdir",
    description = "Create directory <entry> in permissions <mode> (in octal)"
)]
struct MakeDir {
    #[argh(positional, from_str_fn(parse_mode))]
    mode: mode_t,
    #[argh(positional, arg_name = "entry")]
    dir: String,
}

#[derive(FromArgs)]
#[argh(
    subcommand,
    name = "ln",
    description = "Create a symlink to <target> with the name <entry>"
)]
struct Link {
    #[argh(positional, arg_name = "entry")]
    src: String,
    #[argh(positional, arg_name = "target")]
    dst: String,
}

#[derive(FromArgs)]
#[argh(
    subcommand,
    name = "add",
    description = "Add <infile> as <entry> in permissions <mode> (in octal); replace <entry> if exists"
)]
struct Add {
    #[argh(positional, from_str_fn(parse_mode))]
    mode: mode_t,
    #[argh(positional, arg_name = "entry")]
    path: String,
    #[argh(positional, arg_name = "infile")]
    file: String,
}

#[derive(FromArgs)]
#[argh(
    subcommand,
    name = "ls",
    description = r#"List [<path>] ("/" by default); specifly [-r] to recursively list sub-directories"#
)]
struct List {
    #[argh(positional, default = r#"String::from("/")"#)]
    path: String,
    #[argh(switch, short = 'r', description = "recursive")]
    recursive: bool,
}

#[repr(C, packed)]
struct CpioHeader {
    magic: [u8; 6],
    ino: [u8; 8],
    mode: [u8; 8],
    uid: [u8; 8],
    gid: [u8; 8],
    nlink: [u8; 8],
    mtime: [u8; 8],
    filesize: [u8; 8],
    devmajor: [u8; 8],
    devminor: [u8; 8],
    rdevmajor: [u8; 8],
    rdevminor: [u8; 8],
    namesize: [u8; 8],
    check: [u8; 8],
}

pub(crate) struct Cpio {
    pub(crate) entries: BTreeMap<String, Box<CpioEntry>>,
}

pub(crate) struct CpioEntry {
    pub(crate) mode: mode_t,
    pub(crate) uid: uid_t,
    pub(crate) gid: gid_t,
    pub(crate) rdevmajor: dev_t,
    pub(crate) rdevminor: dev_t,
    pub(crate) data: Vec<u8>,
}

impl Cpio {
    fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    fn load_from_data(data: &[u8]) -> LoggedResult<Self> {
        let mut cpio = Cpio::new();
        let mut pos = 0usize;
        while pos < data.len() {
            let hdr = unsafe { &*(data.as_ptr().add(pos) as *const CpioHeader) };
            if &hdr.magic != b"070701" {
                return Err(log_err!("invalid cpio magic"));
            }
            pos += size_of::<CpioHeader>();
            let name = CStr::from_bytes_until_nul(&data[pos..])?
                .to_str()?
                .to_string();
            pos += x8u::<usize>(&hdr.namesize)?;
            pos = align_4(pos);
            if name == "." || name == ".." {
                continue;
            }
            if name == "TRAILER!!!" {
                match data[pos..].windows(6).position(|x| x == b"070701") {
                    Some(x) => pos += x,
                    None => break,
                }
                continue;
            }
            let file_size = x8u::<usize>(&hdr.filesize)?;
            let entry = Box::new(CpioEntry {
                mode: x8u(&hdr.mode)?,
                uid: x8u(&hdr.uid)?,
                gid: x8u(&hdr.gid)?,
                rdevmajor: x8u(&hdr.rdevmajor)?,
                rdevminor: x8u(&hdr.rdevminor)?,
                data: data[pos..pos + file_size].to_vec(),
            });
            pos += file_size;
            cpio.entries.insert(name, entry);
            pos = align_4(pos);
        }
        Ok(cpio)
    }

    pub(crate) fn load_from_file(path: &Utf8CStr) -> LoggedResult<Self> {
        eprintln!("Loading cpio: [{}]", path);
        let file = MappedFile::open(path)?;
        Self::load_from_data(file.as_ref())
    }

    fn dump(&self, path: &str) -> LoggedResult<()> {
        eprintln!("Dumping cpio: [{}]", path);
        let mut file = File::create(path)?;
        let mut pos = 0usize;
        let mut inode = 300000i64;
        for (name, entry) in &self.entries {
            pos += file.write(
                format!(
                    "070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
                    inode,
                    entry.mode,
                    entry.uid,
                    entry.gid,
                    1,
                    0,
                    entry.data.len(),
                    0,
                    0,
                    entry.rdevmajor,
                    entry.rdevminor,
                    name.len() + 1,
                    0
                ).as_bytes(),
            )?;
            pos += file.write(name.as_bytes())?;
            pos += file.write(&[0])?;
            file.write_zeros(align_4(pos) - pos)?;
            pos = align_4(pos);
            pos += file.write(&entry.data)?;
            file.write_zeros(align_4(pos) - pos)?;
            pos = align_4(pos);
            inode += 1;
        }
        pos += file.write(
            format!("070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
                inode, 0o755, 0, 0, 1, 0, 0, 0, 0, 0, 0, 11, 0
            ).as_bytes()
        )?;
        pos += file.write("TRAILER!!!\0".as_bytes())?;
        file.write_zeros(align_4(pos) - pos)?;
        Ok(())
    }

    pub(crate) fn rm(&mut self, path: &str, recursive: bool) {
        let path = norm_path(path);
        if self.entries.remove(&path).is_some() {
            eprintln!("Removed entry [{}]", path);
        }
        if recursive {
            let path = path + "/";
            self.entries.retain(|k, _| {
                if k.starts_with(&path) {
                    eprintln!("Removed entry [{}]", k);
                    false
                } else {
                    true
                }
            })
        }
    }

    fn extract_entry(&self, path: &str, out: &Path) -> LoggedResult<()> {
        let entry = self.entries.get(path).ok_or(log_err!("No such file"))?;
        eprintln!("Extracting entry [{}] to [{}]", path, out.to_string_lossy());
        if let Some(parent) = out.parent() {
            DirBuilder::new()
                .mode(0o755)
                .recursive(true)
                .create(parent)?;
        }
        match entry.mode & S_IFMT {
            S_IFDIR => {
                DirBuilder::new()
                    .mode((entry.mode & 0o777).into())
                    .recursive(true) // avoid error if existing
                    .create(out)?;
            }
            S_IFREG => {
                let mut file = File::create(out)?;
                file.write_all(&entry.data)?;
            }
            S_IFLNK => {
                symlink(Path::new(&std::str::from_utf8(entry.data.as_slice())?), out)?;
            }
            S_IFBLK | S_IFCHR => {
                let dev = makedev(entry.rdevmajor.try_into()?, entry.rdevminor.try_into()?);
                unsafe {
                    mknod(
                        out.to_str().unwrap().as_ptr() as *const c_char,
                        entry.mode,
                        dev,
                    )
                };
            }
            _ => {
                return Err(log_err!("unknown entry type"));
            }
        }
        Ok(())
    }

    fn extract(&self, path: Option<&str>, out: Option<&str>) -> LoggedResult<()> {
        let path = path.map(norm_path);
        let out = out.map(Path::new);
        if let (Some(path), Some(out)) = (&path, &out) {
            return self.extract_entry(path, out);
        } else {
            for path in self.entries.keys() {
                if path == "." || path == ".." {
                    continue;
                }
                self.extract_entry(path, Path::new(path))?;
            }
        }
        Ok(())
    }

    pub(crate) fn exists(&self, path: &str) -> bool {
        self.entries.contains_key(&norm_path(path))
    }

    fn add(&mut self, mode: &mode_t, path: &str, file: &str) -> LoggedResult<()> {
        if path.ends_with('/') {
            return Err(log_err!("path cannot end with / for add"));
        }
        let file = Path::new(file);
        let content = read(file)?;
        let metadata = metadata(file)?;
        let mut rdevmajor: dev_t = 0;
        let mut rdevminor: dev_t = 0;
        let mode = if metadata.file_type().is_file() {
            mode | S_IFREG
        } else {
            rdevmajor = unsafe { major(metadata.rdev().try_into()?).try_into()? };
            rdevminor = unsafe { minor(metadata.rdev().try_into()?).try_into()? };
            if metadata.file_type().is_block_device() {
                mode | S_IFBLK
            } else if metadata.file_type().is_char_device() {
                mode | S_IFCHR
            } else {
                return Err(log_err!("unsupported file type"));
            }
        };
        self.entries.insert(
            norm_path(path),
            Box::new(CpioEntry {
                mode,
                uid: 0,
                gid: 0,
                rdevmajor,
                rdevminor,
                data: content,
            }),
        );
        eprintln!("Add file [{}] ({:04o})", path, mode);
        Ok(())
    }

    fn mkdir(&mut self, mode: &mode_t, dir: &str) {
        self.entries.insert(
            norm_path(dir),
            Box::new(CpioEntry {
                mode: *mode | S_IFDIR,
                uid: 0,
                gid: 0,
                rdevmajor: 0,
                rdevminor: 0,
                data: vec![],
            }),
        );
        eprintln!("Create directory [{}] ({:04o})", dir, mode);
    }

    fn ln(&mut self, src: &str, dst: &str) {
        self.entries.insert(
            norm_path(dst),
            Box::new(CpioEntry {
                mode: S_IFLNK,
                uid: 0,
                gid: 0,
                rdevmajor: 0,
                rdevminor: 0,
                data: norm_path(src).as_bytes().to_vec(),
            }),
        );
        eprintln!("Create symlink [{}] -> [{}]", dst, src);
    }

    fn mv(&mut self, from: &str, to: &str) -> LoggedResult<()> {
        let entry = self
            .entries
            .remove(&norm_path(from))
            .ok_or(log_err!("no such entry {}", from))?;
        self.entries.insert(norm_path(to), entry);
        eprintln!("Move [{}] -> [{}]", from, to);
        Ok(())
    }

    fn ls(&self, path: &str, recursive: bool) {
        let path = norm_path(path);
        let path = if path.is_empty() {
            path
        } else {
            "/".to_string() + path.as_str()
        };
        for (name, entry) in &self.entries {
            let p = "/".to_string() + name.as_str();
            if !p.starts_with(&path) {
                continue;
            }
            let p = p.strip_prefix(&path).unwrap();
            if !p.is_empty() && !p.starts_with('/') {
                continue;
            }
            if !recursive && !p.is_empty() && p.matches('/').count() > 1 {
                continue;
            }
            println!("{}\t{}", entry, name);
        }
    }
}

impl Display for CpioEntry {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}{}{}{}{}{}{}{}{}{}\t{}\t{}\t{}\t{}:{}",
            match self.mode & S_IFMT {
                S_IFDIR => "d",
                S_IFREG => "-",
                S_IFLNK => "l",
                S_IFBLK => "b",
                S_IFCHR => "c",
                _ => "?",
            },
            if self.mode & S_IRUSR != 0 { "r" } else { "-" },
            if self.mode & S_IWUSR != 0 { "w" } else { "-" },
            if self.mode & S_IXUSR != 0 { "x" } else { "-" },
            if self.mode & S_IRGRP != 0 { "r" } else { "-" },
            if self.mode & S_IWGRP != 0 { "w" } else { "-" },
            if self.mode & S_IXGRP != 0 { "x" } else { "-" },
            if self.mode & S_IROTH != 0 { "r" } else { "-" },
            if self.mode & S_IWOTH != 0 { "w" } else { "-" },
            if self.mode & S_IXOTH != 0 { "x" } else { "-" },
            self.uid,
            self.gid,
            Size::from_bytes(self.data.len())
                .format()
                .with_style(Style::Abbreviated)
                .with_base(Base::Base10)
                .to_string(),
            self.rdevmajor,
            self.rdevminor,
        )
    }
}

pub fn cpio_commands(argc: i32, argv: *const *const c_char) -> bool {
    fn inner(argc: i32, argv: *const *const c_char) -> LoggedResult<()> {
        if argc < 1 {
            return Err(log_err!("no arguments"));
        }

        let cmds: Result<Vec<&Utf8CStr>, StrErr> =
            unsafe { slice::from_raw_parts(argv, argc as usize) }
                .iter()
                .map(|s| unsafe { Utf8CStr::from_ptr(*s) })
                .collect();
        let cmds = cmds?;

        let file = cmds[0];
        let mut cpio = if Path::new(file).exists() {
            Cpio::load_from_file(file)?
        } else {
            Cpio::new()
        };
        for cmd in &cmds[1..] {
            if cmd.starts_with('#') {
                continue;
            }
            let mut cli = match CpioCli::from_args(
                &["magiskboot", "cpio", file],
                cmd.split(' ')
                    .filter(|x| !x.is_empty())
                    .collect::<Vec<_>>()
                    .as_slice(),
            ) {
                Ok(cli) => cli,
                Err(EarlyExit { output, status }) => match status {
                    Ok(_) => {
                        eprintln!("{}", output);
                        exit(0)
                    }
                    Err(_) => return Err(log_err!(output)),
                },
            };
            match &mut cli.command {
                CpioCommands::Test(Test {}) => exit(cpio.test()),
                CpioCommands::Restore(Restore {}) => cpio.restore()?,
                CpioCommands::Patch(Patch {}) => cpio.patch(),
                CpioCommands::Exists(Exists { path }) => {
                    if cpio.exists(path) {
                        exit(0);
                    } else {
                        exit(1);
                    }
                }
                CpioCommands::Backup(Backup { origin }) => {
                    cpio.backup(Utf8CStr::from_string(origin))?
                }
                CpioCommands::Remove(Remove { path, recursive }) => cpio.rm(path, *recursive),
                CpioCommands::Move(Move { from, to }) => cpio.mv(from, to)?,
                CpioCommands::MakeDir(MakeDir { mode, dir }) => cpio.mkdir(mode, dir),
                CpioCommands::Link(Link { src, dst }) => cpio.ln(src, dst),
                CpioCommands::Add(Add { mode, path, file }) => cpio.add(mode, path, file)?,
                CpioCommands::Extract(Extract { paths }) => {
                    if !paths.is_empty() && paths.len() != 2 {
                        return Err(log_err!("invalid arguments"));
                    }
                    cpio.extract(
                        paths.get(0).map(|x| x.as_str()),
                        paths.get(1).map(|x| x.as_str()),
                    )?;
                }
                CpioCommands::List(List { path, recursive }) => {
                    cpio.ls(path.as_str(), *recursive);
                    exit(0);
                }
            }
        }
        cpio.dump(file)?;
        Ok(())
    }
    inner(argc, argv)
        .log_with_msg(|w| w.write_str("Failed to process cpio"))
        .is_ok()
}

fn x8u<U: TryFrom<u32>>(x: &[u8; 8]) -> LoggedResult<U> {
    // parse hex
    let mut ret = 0u32;
    for i in x {
        let c = *i as char;
        let v = c.to_digit(16).ok_or(log_err!("bad cpio header"))?;
        ret = ret * 16 + v;
    }
    ret.try_into().map_err(|_| log_err!("bad cpio header"))
}

#[inline(always)]
fn align_4(x: usize) -> usize {
    (x + 3) & !3
}

#[inline(always)]
fn norm_path(path: &str) -> String {
    let path = path.strip_prefix('/').unwrap_or(path);
    path.strip_suffix('/').unwrap_or(path).to_string()
}

fn parse_mode(s: &str) -> Result<mode_t, String> {
    mode_t::from_str_radix(s, 8).map_err(|e| e.to_string())
}
