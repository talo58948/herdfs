//! Direct authority commands, including verified old-version downloads.

use std::fs::File;
use std::io::{self, Seek, SeekFrom};
use std::sync::Arc;

use anyhow::{Context, bail};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Request, Response, Version};

use super::{Command, ConnectArgs, PathArgs, output, version};
use crate::cache::Cache;
use crate::reader::Reader;
use crate::remote::Remote;
use crate::stats::Stats;
use crate::version::OpenVersion;

pub(super) async fn run(command: Command, json: bool) -> anyhow::Result<()> {
    match command {
        Command::Whoami(args) => whoami(&args, json).await,
        Command::Spaces(args) => spaces(&args, json).await,
        Command::History(args) => history_command(&args, json).await,
        Command::Show(args) => show(args, json).await,
        Command::Restore(args) => {
            let request = Request::Restore {
                space: args.path.space.clone(),
                path: path(&args.path.path)?,
                version: version(args.version),
            };
            match remote(&args.path.connect, &args.path.space)?
                .request(request)
                .await?
            {
                Response::Version { version } if json => output::json(&version),
                Response::Version { .. } => Ok(()),
                response => unexpected(&response),
            }
        }
        Command::Share(args) => {
            let request = Request::SetAcl {
                space: args.principal.path.space.clone(),
                path: path(&args.principal.path.path)?,
                principal: args.principal.principal,
                perm: Some(args.perm.into()),
            };
            done(
                &args.principal.path.connect,
                &args.principal.path.space,
                request,
                json,
            )
            .await
        }
        Command::Revoke(args) => {
            let request = Request::SetAcl {
                space: args.path.space.clone(),
                path: path(&args.path.path)?,
                principal: args.principal,
                perm: None,
            };
            done(&args.path.connect, &args.path.space, request, json).await
        }
        Command::Access(args) => access(&args, json).await,
        Command::Fork(args) => {
            let request = Request::Fork {
                space: args.space.clone(),
                name: args.new_name,
            };
            entry(&args.connect, &args.space, request, json).await
        }
        Command::CreateSpace(args) => {
            let request = Request::CreateSpace {
                space: args.space.clone(),
            };
            entry(&args.connect, &args.space, request, json).await
        }
        Command::DeleteSpace(args) => {
            let request = Request::DeleteSpace {
                space: args.space.clone(),
            };
            done(&args.connect, &args.space, request, json).await
        }
        Command::Purge(args) => {
            let request = Request::Purge {
                space: args.space.clone(),
                path: path(&args.path)?,
            };
            done(&args.connect, &args.space, request, json).await
        }
        Command::Mount(_) | Command::Explain { .. } | Command::Login(_) | Command::Logout(_) => {
            bail!("command was dispatched to the wrong handler")
        }
    }
}

fn remote(args: &ConnectArgs, space: &str) -> anyhow::Result<Remote> {
    Remote::new(
        &args.authority,
        space,
        args.token.token_file.clone(),
        &args.resolve,
    )
    .with_context(|| format!("reading token file {}", args.token.token_file.display()))
}

async fn whoami(args: &ConnectArgs, json: bool) -> anyhow::Result<()> {
    match remote(args, "")?.request(Request::Whoami).await? {
        Response::Me { principal, groups } if json => {
            output::json(&serde_json::json!({"principal": principal, "groups": groups}))
        }
        Response::Me { principal, groups } => {
            println!("{principal}");
            for group in groups {
                println!("group:{group}");
            }
            Ok(())
        }
        response => unexpected(&response),
    }
}

async fn spaces(args: &ConnectArgs, json: bool) -> anyhow::Result<()> {
    match remote(args, "")?.request(Request::Spaces).await? {
        Response::Spaces { spaces } => output::spaces(&spaces, json),
        response => unexpected(&response),
    }
}

async fn history_command(args: &PathArgs, json: bool) -> anyhow::Result<()> {
    output::versions(&history(args).await?, json)
}

async fn history(args: &PathArgs) -> anyhow::Result<Vec<Version>> {
    let request = Request::History {
        space: args.space.clone(),
        path: path(&args.path)?,
    };
    match remote(&args.connect, &args.space)?.request(request).await? {
        Response::History { versions } => Ok(versions),
        response => unexpected(&response),
    }
}

async fn show(args: super::ShowArgs, json: bool) -> anyhow::Result<()> {
    if json && args.output.is_none() {
        bail!("--json requires --output for show");
    }
    let wanted = version(args.version);
    let version = history(&args.path)
        .await?
        .into_iter()
        .find(|version| version.id == wanted)
        .with_context(|| format!("version {} is not visible", wanted.0))?;
    let cache_dir = tempfile::tempdir()?;
    let cache = Arc::new(Cache::open(cache_dir.path().into(), version.size.max(1))?);
    let remote = Arc::new(remote(&args.path.connect, &args.path.space)?);
    let reader = Reader::new(remote, cache, Arc::new(Stats::default()));
    let opened = OpenVersion::new(version.clone());
    match args.output {
        Some(path) => {
            let mut file = File::create(&path)?;
            reader.materialize(&opened, &mut file).await?;
            if json {
                output::json(&serde_json::json!({"version": version, "output": path}))?;
            }
        }
        None => {
            let mut file = tempfile::tempfile()?;
            reader.materialize(&opened, &mut file).await?;
            file.seek(SeekFrom::Start(0))?;
            io::copy(&mut file, &mut io::stdout().lock())?;
        }
    }
    Ok(())
}

async fn access(args: &PathArgs, json: bool) -> anyhow::Result<()> {
    let request = Request::Access {
        space: args.space.clone(),
        path: path(&args.path)?,
    };
    match remote(&args.connect, &args.space)?.request(request).await? {
        Response::Access { grants } => output::grants(&grants, json),
        response => unexpected(&response),
    }
}

async fn entry(connect: &ConnectArgs, space: &str, request: Request, json: bool) -> anyhow::Result<()> {
    match remote(connect, space)?.request(request).await? {
        Response::Entry { entry } if json => output::json(&entry),
        Response::Entry { .. } => Ok(()),
        response => unexpected(&response),
    }
}

async fn done(connect: &ConnectArgs, space: &str, request: Request, json: bool) -> anyhow::Result<()> {
    match remote(connect, space)?.request(request).await? {
        Response::Done => output::done(json),
        response => unexpected(&response),
    }
}

fn path(value: &str) -> anyhow::Result<SpacePath> {
    SpacePath::try_from(value).map_err(Into::into)
}

fn unexpected<T>(response: &Response) -> anyhow::Result<T> {
    bail!("unexpected authority response: {response:?}")
}
