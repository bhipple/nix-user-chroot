use nix::mount::{mount, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::unistd;
use std::env;
use std::fs;
use std::io;
use std::io::prelude::*;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::string::String;
use tempdir::TempDir;

const NONE: Option<&'static [u8]> = None;

fn bind_mount_direntry(entry: io::Result<fs::DirEntry>) {
    let entry = entry.expect("error while listing from /nix directory");
    // do not bind mount an existing nix installation
    if entry.file_name() == PathBuf::from("nix") {
        return;
    }
    let path = entry.path();
    let stat = entry
        .metadata()
        .unwrap_or_else(|_| panic!("cannot get stat of {}", path.display()));
    if !stat.is_dir() {
        return;
    }

    let mountpoint = PathBuf::from("/").join(entry.file_name());
    if let Err(e) = fs::create_dir(&mountpoint) {
        if e.kind() != io::ErrorKind::AlreadyExists {
            let e2: io::Result<()> = Err(e);
            e2.unwrap_or_else(|_| panic!("failed to create {}", &mountpoint.display()));
        }
    }

    if let Err(e) = mount(
        Some(&path),
        &mountpoint,
        Some("none"),
        MsFlags::MS_BIND | MsFlags::MS_REC,
        NONE,
    ) {
        eprintln!(
            "failed to bind mount {} to {}: {}",
            path.display(),
            mountpoint.display(),
            e
        );
    }
}

fn run_chroot(nixdir: &Path, rootdir: &Path, cmd: &str, args: &[String]) {
    let cwd = env::current_dir().expect("cannot get current working directory");

    let uid = unistd::getuid();
    let gid = unistd::getgid();

    unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWUSER).expect("unshare failed");

    // fixes issue #1 where writing to /proc/self/gid_map fails
    // see user_namespaces(7) for more documentation
    if let Ok(mut file) = fs::File::create("/proc/self/setgroups") {
        let _ = file.write_all(b"deny");
    }

    let mut uid_map =
        fs::File::create("/proc/self/uid_map").expect("failed to open /proc/self/uid_map");
    uid_map
        .write_all(format!("{} {} 1", uid, uid).as_bytes())
        .expect("failed to write new uid mapping to /proc/self/uid_map");

    let mut gid_map =
        fs::File::create("/proc/self/gid_map").expect("failed to open /proc/self/gid_map");
    gid_map
        .write_all(format!("{} {} 1", gid, gid).as_bytes())
        .expect("failed to write new gid mapping to /proc/self/gid_map");

    // prepare pivot_root call:
    // rootdir must be a mount point
    let opts = format!("size=1M,mode=700,nr_inodes=10k,uid={},gid={}", uid, gid);
    mount(
        Some("tmpfs"),
        rootdir,
        Some("tmpfs"),
        MsFlags::empty(),
        Some(opts.as_str()),
    ).expect("failed to mount tmpfs for new chroot");

    mount(
        Some(rootdir),
        rootdir,
        Some("none"),
        MsFlags::MS_PRIVATE | MsFlags::MS_REC,
        NONE,
    ).expect("failed to make tmpfs chroot a private mount");

    // create the mount point for the old root
    // The old root cannot be unmounted/removed after pivot_root, the only way to
    // keep / clean is to hide the directory with another mountpoint. Therefore
    // we pivot the old root to /nix. This is somewhat confusing, though.
    let nix_mountpoint = rootdir.join("nix");
    fs::create_dir(&nix_mountpoint)
        .unwrap_or_else(|_| panic!("failed to create {}", &nix_mountpoint.display()));

    unistd::pivot_root(rootdir, &nix_mountpoint).unwrap_or_else(|_| {
        panic!(
            "pivot_root({},{})",
            rootdir.display(),
            nix_mountpoint.display()
        )
    });

    env::set_current_dir("/").expect("cannot change directory to /");

    // bind mount all / stuff into rootdir
    // the orginal content of / now available under /nix
    let nix_root = PathBuf::from("/nix");
    let dir = fs::read_dir(&nix_root).expect("failed to list /nix directory");
    for entry in dir {
        bind_mount_direntry(entry);
    }
    // mount the store and hide the old root
    // we fetch nixdir under the old root
    let nix_store = nix_root.join(nixdir);
    mount(
        Some(&nix_store),
        "/nix",
        Some("none"),
        MsFlags::MS_BIND | MsFlags::MS_REC,
        NONE,
    )
    .unwrap_or_else(|_| panic!("failed to bind mount {} to /nix", nix_store.display()));

    // restore cwd
    env::set_current_dir(&cwd)
        .unwrap_or_else(|_| panic!("cannot restore working directory {}", cwd.display()));

    let err = process::Command::new(&cmd)
        .args(args)
        .env("NIX_CONF_DIR", "/nix/etc/nix")
        .exec();

    eprintln!("failed to execute {}: {}", &cmd, err);
    process::exit(1);
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <nixpath> <command>\n", args[0]);
        process::exit(1);
    }
    let tempdir =
        TempDir::new("nix").expect("failed to create temporary directory for mount point");
    let rootdir = PathBuf::from(tempdir.path());

    let nixdir = fs::canonicalize(&args[1])
        .unwrap_or_else(|_| panic!("failed to resolve nix directory {}", &args[1]));

    run_chroot(&nixdir, &rootdir, &args[2], &args[3..]);
}
