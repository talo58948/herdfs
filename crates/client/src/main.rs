//! The `herdfs` command.

fn main() -> std::process::ExitCode {
    match herdfs_client::cli::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            if !error.is::<herdfs_client::cli::PrintedError>() {
                eprintln!("Error: {error:#}");
            }
            std::process::ExitCode::FAILURE
        }
    }
}
