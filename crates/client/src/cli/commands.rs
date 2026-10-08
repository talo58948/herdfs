//! Direct authority commands, including verified old-version downloads.

use std::io::{self, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, bail};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Request, Response, Version, VersionId};

use super::{AuthorityCommand, ConnectArgs, PathArgs, output};
use crate::cache::Cache;
use crate::reader::Reader;
use crate::remote::Remote;
use crate::stats::Stats;
use crate::version::OpenVersion;

pub(super) async fn run(command: AuthorityCommand, json: bool) -> anyhow::Result<()> {
    match command {
        AuthorityCommand::Whoami(args) => whoami(&args, json).await,
        AuthorityCommand::Spaces(args) => spaces(&args, json).await,
        AuthorityCommand::History(args) => print_history(&args, json).await,
        AuthorityCommand::Show(args) => show(&args, json).await,
        AuthorityCommand::Restore(args) => restore(&args, json).await,
        AuthorityCommand::Share(args) => share(&args, json).await,
        AuthorityCommand::Revoke(args) => revoke(&args, json).await,
        AuthorityCommand::Access(args) => access(&args, json).await,
        AuthorityCommand::Fork(args) => fork(&args, json).await,
        AuthorityCommand::CreateSpace(args) => create_space(&args, json).await,
        AuthorityCommand::DeleteSpace(args) => delete_space(&args, json).await,
        AuthorityCommand::Purge(args) => purge(&args, json).await,
    }
}

fn remote(args: &ConnectArgs, space: &str) -> anyhow::Result<Remote> {
    let token_file = args.token.path()?;
    crate::token::read_access_token(&token_file)
        .with_context(|| format!("reading token file {}", token_file.display()))?;
    Ok(Remote::new(&args.authority, space, token_file, &args.resolve)?)
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

async fn print_history(args: &PathArgs, json: bool) -> anyhow::Result<()> {
    let remote = remote(&args.connect, &args.space)?;
    output::versions(&history(&remote, args).await?, json)
}

async fn history(remote: &Remote, args: &PathArgs) -> anyhow::Result<Vec<Version>> {
    let request = Request::History {
        space: args.space.clone(),
        path: path(&args.path)?,
    };
    match remote.request(request).await? {
        Response::History { versions } => Ok(versions),
        response => unexpected(&response),
    }
}

async fn show(args: &super::ShowArgs, json: bool) -> anyhow::Result<()> {
    if json && args.output.is_none() {
        bail!("--json requires --output for show");
    }
    let path_args = &args.version.path;
    let remote = Arc::new(remote(&path_args.connect, &path_args.space)?);
    let wanted = VersionId(args.version.version);
    let version = history(&remote, path_args)
        .await?
        .into_iter()
        .find(|version| version.id == wanted)
        .with_context(|| format!("version {} is not visible", wanted.0))?;
    let cache_dir = tempfile::tempdir()?;
    let cache = Arc::new(Cache::open(cache_dir.path().into(), version.size.max(1), &[])?);
    let reader = Reader::new(remote, cache, Arc::new(Stats::default()));
    let opened = OpenVersion::new(version.clone());
    match &args.output {
        Some(path) => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
            reader.materialize(&opened, temporary.as_file_mut()).await?;
            temporary.persist(path).map_err(|error| error.error)?;
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

async fn restore(args: &super::VersionArgs, json: bool) -> anyhow::Result<()> {
    let request = Request::Restore {
        space: args.path.space.clone(),
        path: path(&args.path.path)?,
        version: VersionId(args.version),
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

async fn share(args: &super::ShareArgs, json: bool) -> anyhow::Result<()> {
    let path_args = &args.principal.path;
    let request = Request::SetAcl {
        space: path_args.space.clone(),
        path: path(&path_args.path)?,
        principal: args.principal.principal.clone(),
        perm: Some(args.perm.into()),
    };
    done(&path_args.connect, &path_args.space, request, json).await
}

async fn revoke(args: &super::PrincipalArgs, json: bool) -> anyhow::Result<()> {
    let request = Request::SetAcl {
        space: args.path.space.clone(),
        path: path(&args.path.path)?,
        principal: args.principal.clone(),
        perm: None,
    };
    done(&args.path.connect, &args.path.space, request, json).await
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

async fn fork(args: &super::ForkArgs, json: bool) -> anyhow::Result<()> {
    let request = Request::Fork {
        space: args.space.clone(),
        name: args.new_name.clone(),
    };
    entry(&args.connect, &args.space, request, json).await
}

async fn create_space(args: &super::SpaceArgs, json: bool) -> anyhow::Result<()> {
    let request = Request::CreateSpace {
        space: args.space.clone(),
    };
    entry(&args.connect, &args.space, request, json).await
}

async fn delete_space(args: &super::SpaceArgs, json: bool) -> anyhow::Result<()> {
    let request = Request::DeleteSpace {
        space: args.space.clone(),
    };
    done(&args.connect, &args.space, request, json).await
}

async fn purge(args: &PathArgs, json: bool) -> anyhow::Result<()> {
    let request = Request::Purge {
        space: args.space.clone(),
        path: path(&args.path)?,
    };
    done(&args.connect, &args.space, request, json).await
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
