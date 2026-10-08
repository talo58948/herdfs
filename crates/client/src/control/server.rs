use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, ensure};
use herdfs_core::SpacePath;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::registry::{Registered, Registration};
use crate::fs::Space;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Status,
    Explain { path: PathBuf },
    Stats,
    Pin { path: PathBuf },
    Unpin { path: PathBuf },
    Pause,
    Resume,
    File { path: PathBuf },
}

pub(crate) struct Guard {
    socket: PathBuf,
    _registered: Registered,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
    }
}

pub(crate) fn serve(
    space: Arc<Space>,
    authority: &str,
    space_name: &str,
    mountpoint: &Path,
    state: &Path,
) -> anyhow::Result<Guard> {
    fs::create_dir_all(state)?;
    let socket = state.join("control.sock");
    if socket.exists() {
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let mountpoint = fs::canonicalize(mountpoint)
        .with_context(|| format!("resolving mountpoint {}", mountpoint.display()))?;
    let registration = Registration {
        id: format!("{:032x}", rand::random::<u128>()),
        space: space_name.to_owned(),
        authority: authority.to_owned(),
        mountpoint,
        socket: socket.clone(),
        pid: std::process::id(),
    };
    let registered = Registered::write(&registration)?;
    tokio::spawn(run(listener, space, registration));
    Ok(Guard {
        socket,
        _registered: registered,
    })
}

async fn run(listener: UnixListener, space: Arc<Space>, mount: Registration) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                tracing::warn!(%error, "control socket accept failed");
                continue;
            }
        };
        let space = space.clone();
        let mount = mount.clone();
        tokio::spawn(async move {
            if let Err(error) = connection(stream, space, mount).await {
                tracing::warn!(%error, "control connection failed");
            }
        });
    }
}

async fn connection(stream: UnixStream, space: Arc<Space>, mount: Registration) -> anyhow::Result<()> {
    ensure!(
        stream.peer_cred()?.uid() == rustix::process::geteuid().as_raw(),
        "control peer is not the mounting user"
    );
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str(&line) {
            Ok(request) => answer(&space, &mount, request)
                .await
                .unwrap_or_else(|error| failure(&error)),
            Err(error) => failure(&error.into()),
        };
        write.write_all(&serde_json::to_vec(&response)?).await?;
        write.write_all(b"\n").await?;
    }
    Ok(())
}

async fn answer(space: &Space, mount: &Registration, request: Request) -> anyhow::Result<Value> {
    match request {
        Request::Status => {
            let (kept_bytes, pins) = space.kept.status(space);
            let pending: Vec<_> = space
                .pending()
                .into_iter()
                .map(|(path, bytes)| json!({"path":path,"bytes":bytes}))
                .collect();
            Ok(json!({
                "ok": true, "space": mount.space, "mountpoint": mount.mountpoint,
                "connected": space.remote.link.connected(),
                "round_trip_ms": space.remote.round_trip.get().map(|seconds| (seconds * 1e3).round() as u64),
                "paused": *space.commits.borrow(), "pending": pending,
                "cache": {"used": space.cache.used(), "budget": space.cache.capacity()},
                "kept": {"bytes": kept_bytes, "pins": pins.into_iter().map(|(path, ready)| json!({"path":path,"ready":ready})).collect::<Vec<_>>()}
            }))
        }
        Request::Stats => with_ok(serde_json::from_slice(&space.stats())?),
        Request::Explain { path } => {
            let path = relative(mount, &path)?;
            let value = space
                .explain_path(&path)
                .context("path has not been read since mount")?;
            with_ok(serde_json::from_slice(&value)?)
        }
        Request::Pin { path } => {
            space.kept.pin(&relative(mount, &path)?)?;
            Ok(json!({"ok":true}))
        }
        Request::Unpin { path } => {
            space.kept.unpin(space, &relative(mount, &path)?)?;
            Ok(json!({"ok":true}))
        }
        Request::Pause => {
            space.commits.send_replace(true);
            Ok(json!({"ok":true}))
        }
        Request::Resume => {
            space.commits.send_replace(false);
            Ok(json!({"ok":true}))
        }
        Request::File { path } => {
            let path = relative(mount, &path)?;
            let (kept, downloading, error) = space.kept.file(&path);
            let pending = space.inodes.at(&path).is_some_and(|(entry, _, _)| {
                space
                    .inodes
                    .local(entry.id, |local| local.is_pending())
                    .unwrap_or(false)
            });
            Ok(json!({"ok":true,"kept":kept,"downloading":downloading,"pending":pending,"error":error}))
        }
    }
}

fn relative(mount: &Registration, path: &Path) -> anyhow::Result<SpacePath> {
    ensure!(path.is_absolute(), "path is not absolute");
    ensure!(
        path.components().all(|component| matches!(
            component,
            std::path::Component::RootDir | std::path::Component::Normal(_)
        )),
        "path contains a non-normal component"
    );
    let relative = path
        .strip_prefix(&mount.mountpoint)
        .with_context(|| format!("{} is outside {}", path.display(), mount.mountpoint.display()))?;
    let names: Option<Vec<_>> = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect();
    Ok(SpacePath::try_from(
        names.context("path is not UTF-8")?.join("/"),
    )?)
}

fn with_ok(mut value: Value) -> anyhow::Result<Value> {
    value
        .as_object_mut()
        .context("control response is not an object")?
        .insert("ok".into(), true.into());
    Ok(value)
}

fn failure(error: &anyhow::Error) -> Value {
    json!({"ok":false,"error":error.to_string()})
}

#[cfg(test)]
mod tests {
    use super::*;

    // contract: U1, U2
    #[tokio::test]
    async fn peer_credentials_name_the_local_user() {
        let (one, _two) = UnixStream::pair().unwrap();
        assert_eq!(
            one.peer_cred().unwrap().uid(),
            rustix::process::geteuid().as_raw()
        );
    }

    // contract: U2
    #[test]
    fn control_paths_are_absolute_and_normal() {
        let mount = Registration {
            id: "test".into(),
            space: "space".into(),
            authority: "http://authority".into(),
            mountpoint: "/mount".into(),
            socket: "/socket".into(),
            pid: 1,
        };
        assert_eq!(
            relative(&mount, Path::new("/mount/file")).unwrap().as_str(),
            "file"
        );
        assert!(
            relative(&mount, Path::new("mount/file"))
                .unwrap_err()
                .to_string()
                .contains("not absolute")
        );
        assert!(
            relative(&mount, Path::new("/mount/../outside"))
                .unwrap_err()
                .to_string()
                .contains("non-normal")
        );
    }
}
