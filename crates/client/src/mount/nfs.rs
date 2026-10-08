//! Loopback NFS mount lifecycle and platform commands.

use std::fs;
#[cfg(target_os = "macos")]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::Arc;

use anyhow::Context;
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use tokio::signal::unix::{SignalKind, signal};

use super::{Connected, MountArgs, connect, serve_control};
use crate::fs::{Identity, Nfs};

pub(super) fn mount(args: &MountArgs) -> anyhow::Result<()> {
    // Space's journal paths use block_in_place, so NFS always owns a multi-thread runtime.
    let runtime = tokio::runtime::Runtime::new()?;
    let _context = runtime.enter();
    let Connected {
        space,
        journal,
        state,
    } = runtime.block_on(connect(args))?;
    let identity = Arc::new(Identity::open(&journal)?);
    let address = format!("127.0.0.1:{}", identity.port());
    let nfs = Nfs::new(space.clone(), identity.clone());
    let listener = runtime
        .block_on(NFSTcpListener::bind(&address, nfs))
        .with_context(|| format!("binding the loopback NFS server at {address}"))?;
    let port = listener.get_listen_port();
    let mut server = runtime.spawn(async move { listener.handle_forever().await });
    if identity.port() == 0 {
        identity.set_port(port)?;
    }
    let mounted = loopback_mount(&args.mountpoint);
    if !mounted {
        fs::create_dir_all(&args.mountpoint)?;
        #[cfg(target_os = "macos")]
        {
            if let Some(parent) = args.mountpoint.parent() {
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
            if let Err(error) = platform_mount(&args.mountpoint, port) {
                server.abort();
                return Err(error);
            }
        }
        #[cfg(target_os = "linux")]
        println!("{}", linux_mount_command(&args.mountpoint, port));
    }
    let _control = serve_control(args, space, &state)?;
    tracing::info!(mountpoint = %args.mountpoint.display(), space = args.space, port, mounted, "serving NFS");
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let server_stopped = runtime.block_on(async {
        tokio::select! {
            _ = interrupt.recv() => None,
            _ = terminate.recv() => None,
            result = &mut server => Some(anyhow::anyhow!("NFS server stopped: {result:?}")),
        }
    });
    #[cfg(target_os = "macos")]
    let unmounted = if server_stopped.is_some() {
        platform_unmount(&args.mountpoint, true)
    } else {
        platform_unmount(&args.mountpoint, false)
            .or_else(|error| platform_unmount(&args.mountpoint, true).context(error))
    };
    server.abort();
    if let Some(error) = server_stopped {
        return Err(error);
    }
    #[cfg(target_os = "macos")]
    unmounted?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_mount(mountpoint: &std::path::Path, port: u16) -> anyhow::Result<()> {
    let options = format!(
        "vers=3,tcp,hard,intr,locallocks,noquota,rdirplus,actimeo=1,nonegnamecache,readahead=0,rsize=131072,wsize=131072,port={port},mountport={port}"
    );
    run_mount(
        Command::new("/sbin/mount_nfs")
            .args(["-o", &options, "127.0.0.1:/"])
            .arg(mountpoint),
    )
}

#[cfg(target_os = "linux")]
fn linux_mount_command(mountpoint: &std::path::Path, port: u16) -> String {
    let options = format!(
        "vers=3,tcp,nolock,noacl,actimeo=1,lookupcache=none,rsize=131072,wsize=131072,port={port},mountport={port}"
    );
    format!("sudo mount.nfs -o {options} 127.0.0.1:/ {}", mountpoint.display())
}

#[cfg(target_os = "macos")]
fn platform_unmount(mountpoint: &std::path::Path, force: bool) -> anyhow::Result<()> {
    let mut command = Command::new("/sbin/umount");
    if force {
        command.arg("-f");
    }
    run_mount(command.arg(mountpoint))
}

#[cfg(target_os = "macos")]
fn run_mount(command: &mut Command) -> anyhow::Result<()> {
    let shown = format!("{command:?}");
    let status = command.status().with_context(|| format!("running {shown}"))?;
    anyhow::ensure!(status.success(), "{shown} failed with {status}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn loopback_mount(path: &std::path::Path) -> bool {
    let Ok(stat) = rustix::fs::statfs(path) else {
        return false;
    };
    let fs_type = stat.f_fstypename.map(|byte| byte as u8);
    if !fs_type.starts_with(b"nfs\0") {
        return false;
    }
    let output = Command::new("/sbin/mount").output();
    let Ok(output) = output else {
        return false;
    };
    let needle = format!(" on {} (", path.display());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.starts_with("127.0.0.1:") && line.contains(&needle))
}

#[cfg(target_os = "linux")]
fn loopback_mount(path: &std::path::Path) -> bool {
    if rustix::fs::statfs(path).map(|stat| stat.f_type) != Ok(libc::NFS_SUPER_MAGIC as _) {
        return false;
    }
    let output = Command::new("findmnt")
        .args(["-n", "-o", "SOURCE", "--target"])
        .arg(path)
        .output();
    let Ok(output) = output else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout)
        .trim_start()
        .starts_with("127.0.0.1:")
}
