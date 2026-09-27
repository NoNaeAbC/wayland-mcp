use std::env;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use tokio::process::Command;

pub(crate) fn command(runtime: &str) -> Result<Command, String> {
    let node = env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("node"))
        .find(|path| {
            path.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .ok_or("JavaScript console requires Node on PATH")?;
    let mut command = Command::new(node);
    command
        .args([
            "--permission",
            "--no-addons",
            "--disable-sigusr1",
            "--max-old-space-size=128",
            "--input-type=module",
            "--eval",
            runtime,
        ])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}
