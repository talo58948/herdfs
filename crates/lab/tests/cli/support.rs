use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};
use herdfs_lab::{Lab, herdfs_command};

pub fn command(lab: &Lab, client: &str, token: &Path, arguments: &[&str]) -> Result<Command> {
    let authority = lab.authority_url(client);
    let url = reqwest::Url::parse(&authority)?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("authority URL has no host"))?;
    let mut command = herdfs_command()?;
    command
        .args(arguments)
        .args(["--authority", &authority, "--token-file"]);
    command.arg(token).args(["--resolve", &format!("s3.lab={host}")]);
    Ok(command)
}

pub fn json(command: &mut Command) -> Result<serde_json::Value> {
    command.arg("--json");
    let output = command.output()?;
    if !output.status.success() {
        bail!(
            "command failed: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}
