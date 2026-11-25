use crate::ssh_video;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

pub(crate) struct RemoteExecutable {
    path: String,
    // The staging channel keeps this session's executable pinned. Closing its
    // stdin lets the remote shell clean up after the worker exits.
    _lease: ChildStdin,
    _ssh: Child,
}

impl RemoteExecutable {
    pub(crate) fn environment(&self) -> String {
        let directory = self.path.strip_suffix("/wayland-mcp").unwrap();
        format!(
            "export WAYLAND_MCP_EXECUTABLE='{}'\nexport PATH='{directory}':\"$PATH\"\n",
            self.path
        )
    }
}

pub(crate) async fn stage_executable(
    destination: &OsStr,
    config: Option<&OsStr>,
    batch: bool,
) -> Result<RemoteExecutable, String> {
    // Open the executable of this process even if a rebuild has replaced it
    // on disk while the launcher is running.
    let executable = tokio::fs::read("/proc/self/exe")
        .await
        .map_err(|error| format!("read the launcher's executable: {error}"))?;
    let digest = executable_digest(&executable)?;
    let script = stage_script(executable.len(), &digest, "/tmp/wayland-mcp-cache.$uid");
    let mut command = Command::new("ssh");
    if let Some(config) = config {
        command.arg("-F").arg(config);
    }
    command.args([
        "-T",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "Compression=yes",
    ]);
    if batch {
        command.args(["-o", "BatchMode=yes"]);
    }
    let mut child = command
        .arg("--")
        .arg(destination)
        .arg(format!("sh -c {}", quote(script.into())?))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("start SSH executable staging: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("missing SSH staging stdin")?;
    let stdout = child.stdout.take().ok_or("missing SSH staging stdout")?;
    let request = format!("wayland-mcp upload {digest}");
    let mut uploaded = false;
    let mut lines = BufReader::new(stdout).take(4096).lines();
    while let Some(line) = lines.next_line().await.map_err(|error| error.to_string())? {
        if line == request {
            if uploaded {
                return Err("SSH requested the executable more than once".into());
            }
            stdin
                .write_all(&executable)
                .await
                .map_err(|error| format!("upload SSH executable: {error}"))?;
            uploaded = true;
        } else if line
            .strip_prefix("/tmp/wayland-mcp-ssh.")
            .and_then(|rest| rest.strip_suffix("/wayland-mcp"))
            .is_some_and(|nonce| {
                nonce.len() == 8 && nonce.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
        {
            return Ok(RemoteExecutable {
                path: line,
                _lease: stdin,
                _ssh: child,
            });
        }
    }
    Err("SSH did not report a staged wayland-mcp executable".into())
}

fn executable_digest(executable: &[u8]) -> Result<String, String> {
    let digest = ssh_video::sha256(executable)?;
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(encoded, "{byte:02x}").unwrap();
    }
    Ok(encoded)
}

fn stage_script(size: usize, digest: &str, cache_path: &str) -> String {
    format!(
        r#"set -eu
umask 077
work=$(mktemp -d /tmp/wayland-mcp-ssh.XXXXXXXX)
install=''
trap 'if [ -n "$install" ]; then rm -f -- "$install/wayland-mcp"; rmdir -- "$install"; fi
    rm -f -- "$work/wayland-mcp"; rmdir -- "$work"' EXIT
trap 'exit 1' HUP INT TERM
bin="$work/wayland-mcp"
uid=$(id -u)
cache={cache_path}
cached="$cache/wayland-mcp"
matches() {{
    [ "$(sha256sum <"$bin")" = '{digest}  -' ]
}}
cache_ready=false
if command -v sha256sum >/dev/null 2>&1; then
    mkdir -m 700 -- "$cache" 2>/dev/null || :
    if [ -d "$cache" ] && [ ! -L "$cache" ] &&
        [ "$(stat -c '%u:%a' -- "$cache" 2>/dev/null)" = "$uid:700" ]; then
        cache_ready=true
    fi
fi
if "$cache_ready" && ln -P -- "$cached" "$bin" 2>/dev/null &&
    [ -f "$bin" ] && [ ! -L "$bin" ] &&
    [ "$(stat -c '%u:%a' -- "$bin")" = "$uid:700" ] && matches; then
    :
else
    rm -f -- "$bin"
    printf '%s\n' 'wayland-mcp upload {digest}'
    head -c {size} >"$bin"
    test "$(wc -c <"$bin")" -eq {size}
    if command -v sha256sum >/dev/null 2>&1; then
        matches
    fi
    chmod 700 "$bin"
    if "$cache_ready" && install=$(mktemp -d "$cache/install.XXXXXXXX"); then
        if ln -- "$bin" "$install/wayland-mcp"; then
            mv -fT -- "$install/wayland-mcp" "$cached" || :
        fi
    fi
fi
printf '%s\n' "$bin"
cat >/dev/null"#
    )
}

pub(crate) async fn run(args: impl Iterator<Item = OsString>) -> Result<(), String> {
    let mut args = args;
    let target = args.next().ok_or("--ssh requires a destination")?;
    let mut config = None;
    let mut display = None;
    let mut local_js = false;
    while let Some(flag) = args.next() {
        if flag == "--local-js" {
            local_js = true;
            continue;
        }
        let value = args
            .next()
            .ok_or_else(|| format!("{} requires a value", flag.to_string_lossy()))?;
        match flag.to_str() {
            Some("--remote-display") => display = Some(value),
            Some("--ssh-config") => config = Some(value),
            _ => return Err(format!("unknown SSH option {}", flag.to_string_lossy())),
        }
    }
    let probe_decoders = async {
        tokio::task::spawn_blocking(ssh_video::local_decoders)
            .await
            .map_err(|error| format!("probe local H.265 decoders: {error}"))
    };
    let (decoders, worker) = tokio::try_join!(
        probe_decoders,
        stage_executable(&target, config.as_deref(), true),
    )?;
    let capabilities = serde_json::to_string(decoders).map_err(|error| error.to_string())?;
    let mut remote = worker.environment();
    remote.push_str("export ");
    remote.push_str(ssh_video::SSH_DECODERS_ENV);
    remote.push('=');
    remote.push_str(&quote(capabilities.into())?);
    remote.push('\n');
    if let Some(display) = display {
        remote.push_str("export WAYLAND_DISPLAY=");
        remote.push_str(&quote(display)?);
        remote.push('\n');
    }
    let mut command = Command::new("ssh");
    command.args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
    let local_console = if local_js {
        let directory = tempfile::Builder::new()
            .prefix("wayland-js-")
            .rand_bytes(20)
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in("/tmp")
            .map_err(|error| format!("create local runtime directory: {error}"))?;
        let socket = directory.path().join("runtime.sock");
        let listener = tokio::net::UnixListener::bind(&socket)
            .map_err(|error| format!("listen for tunneled runtime: {error}"))?;
        let remote_socket = format!(
            "/tmp/{}.sock",
            directory.path().file_name().unwrap().to_string_lossy()
        );
        command
            .args(["-o", "ExitOnForwardFailure=yes", "-R"])
            .arg(format!("{remote_socket}:{}", socket.display()));
        let remove = quote(OsString::from(format!("rm -f -- {remote_socket}")))?;
        remote.push_str(&format!("trap {remove} EXIT\n"));
        remote.push_str("\"$WAYLAND_MCP_EXECUTABLE\"");
        remote.push_str(" --ssh-worker ");
        remote.push_str(&quote(OsString::from(remote_socket))?);
        Some((directory, listener))
    } else {
        command.args(["-o", "ClearAllForwardings=yes"]);
        remote.push_str("exec ");
        remote.push_str("\"$WAYLAND_MCP_EXECUTABLE\"");
        remote.push_str(" --ssh-worker");
        None
    };
    if let Some(config) = config {
        command.arg("-F").arg(config);
    }
    // The local MCP client can terminate this launcher while an SSH channel is open.
    // Tie the child lifetime to the launcher even when Rust destructors cannot run.
    let launcher_pid = std::process::id();
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGTERM as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() as u32 != launcher_pid {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "SSH launcher exited",
                ));
            }
            Ok(())
        });
    }
    let mut child = command
        .arg("--")
        .arg(target)
        .arg(remote)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("start SSH: {error}"))?;
    let mut to_ssh = child.stdin.take().ok_or("missing SSH stdin")?;
    let from_ssh = child.stdout.take().ok_or("missing SSH stdout")?;
    let forward_input = async {
        let result = tokio::io::copy(&mut tokio::io::stdin(), &mut to_ssh).await;
        match result {
            Ok(_) => to_ssh.shutdown().await,
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
            Err(error) => Err(error),
        }
    };
    let forward_output = forward_results(BufReader::new(from_ssh), tokio::io::stdout());
    let console = async {
        if let Some((_directory, listener)) = local_console {
            serve_local_console(listener).await
        } else {
            std::future::pending().await
        }
    };
    tokio::pin!(forward_output, console);
    let input_closed = tokio::select! {
        result = &mut console => {
            result.map_err(|error| format!("local SSH JavaScript runtime: {error}"))?;
            false
        }
        result = &mut forward_output => {
            result.map_err(|error| format!("receive SSH MCP data: {error}"))?;
            false
        }
        result = forward_input => {
            result.map_err(|error| format!("send SSH MCP data: {error}"))?;
            true
        }
    };
    if input_closed {
        // Keep serving the reverse runtime channel while the remote worker drains.
        let graceful = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                result = &mut forward_output => result,
                result = &mut console => result,
            }
            .map_err(|error| format!("drain SSH MCP session: {error}"))?;
            child
                .wait()
                .await
                .map_err(|error| format!("wait for SSH: {error}"))?;
            Ok::<(), String>(())
        })
        .await;
        return match graceful {
            Ok(result) => result,
            Err(_) => child
                .kill()
                .await
                .map_err(|error| format!("close SSH: {error}")),
        };
    }
    let status = child
        .wait()
        .await
        .map_err(|error| format!("wait for SSH: {error}"))?;
    if !status.success() {
        return Err(format!("SSH MCP session ended with {status}"));
    }
    Ok(())
}

pub(crate) fn quote(value: OsString) -> Result<String, String> {
    let value = value
        .into_string()
        .map_err(|_| "remote options must be UTF-8")?;
    if value.is_empty() || value.contains('\0') {
        return Err("remote options must be nonempty and contain no NUL".into());
    }
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

const MAX_MCP_LINE: u64 = 64 * 1024 * 1024;

async fn forward_results(
    mut input: impl tokio::io::AsyncBufRead + Unpin,
    mut output: impl tokio::io::AsyncWrite + Unpin,
) -> io::Result<()> {
    let mut data = Vec::new();
    loop {
        data.clear();
        if (&mut input)
            .take(MAX_MCP_LINE + 1)
            .read_until(b'\n', &mut data)
            .await?
            == 0
        {
            return Ok(());
        }
        if data.len() as u64 > MAX_MCP_LINE || data.last() != Some(&b'\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete or oversized SSH MCP message",
            ));
        }
        if data
            .windows(ssh_video::MIME.len())
            .any(|window| window == ssh_video::MIME.as_bytes())
        {
            data = tokio::task::spawn_blocking(move || decode_result(data))
                .await
                .map_err(io::Error::other)??;
        }
        output.write_all(&data).await?;
        output.flush().await?;
    }
}

fn decode_result(data: Vec<u8>) -> io::Result<Vec<u8>> {
    let mut message: Value = serde_json::from_slice(&data)?;
    let Some(result) = message.get_mut("result") else {
        return Ok(data);
    };
    let Some(content) = result.get("content").and_then(Value::as_array) else {
        return Ok(data);
    };
    if !content
        .iter()
        .any(|entry| entry.get("mimeType").and_then(Value::as_str) == Some(ssh_video::MIME))
    {
        return Ok(data);
    }

    if let Err(error) = decode_images(result) {
        *result = serde_json::to_value(crate::tool_error(format!(
            "SSH video frame rejected: {error}"
        )))?;
    }
    let mut output = serde_json::to_vec(&message)?;
    output.push(b'\n');
    Ok(output)
}

fn decode_images(result: &mut Value) -> Result<(), String> {
    let mut structured = result
        .get_mut("structuredContent")
        .ok_or("missing video response metadata")?
        .take();
    let metadata = structured
        .get_mut("images")
        .and_then(Value::as_array_mut)
        .ok_or("missing video image metadata")?;
    let content = result
        .get_mut("content")
        .and_then(Value::as_array_mut)
        .ok_or("missing video response content")?;
    let mut images = content
        .iter_mut()
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("image"));
    let mut total = 0;
    for entry in metadata {
        let image = images
            .next()
            .ok_or("video result has fewer images than metadata entries")?;
        if image.get("mimeType").and_then(Value::as_str) != Some(ssh_video::MIME) {
            continue;
        }
        let encoded = image
            .get("data")
            .and_then(Value::as_str)
            .ok_or("missing HEVC access unit")?;
        if encoded.len() > ssh_video::MAX_ACCESS_UNIT.div_ceil(3) * 4 {
            return Err("oversized encoded HEVC access unit".into());
        }
        let packet = entry
            .get_mut("packet")
            .ok_or("missing H.265 packet metadata")?
            .take();
        let packet = serde_json::from_value(packet).map_err(|error| error.to_string())?;
        let mut color = entry
            .get_mut("color")
            .ok_or("missing H.265 frame color metadata")?
            .take();
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|error| error.to_string())?;
        let png = ssh_video::decode(&bytes, packet, &mut color)?;
        total += png.len();
        if total > 32 * 1024 * 1024 {
            return Err("decoded video exceeds the evaluation image budget".into());
        }
        let (png, prepared) = crate::prepare_png_image(png, Some(color));
        *entry = prepared;
        *image = json!({"type": "image", "mimeType": "image/png", "data": STANDARD.encode(png)});
    }
    if images.next().is_some() {
        return Err("video result has more images than metadata entries".into());
    }
    let text = content.first_mut().ok_or("missing video result text")?;
    *text = json!({"type": "text", "text": serde_json::to_string_pretty(&structured).map_err(|error| error.to_string())?});
    result["structuredContent"] = structured;
    Ok(())
}

async fn serve_local_console(listener: tokio::net::UnixListener) -> io::Result<()> {
    loop {
        let (socket, _) = listener.accept().await?;
        let mut command = crate::console_runtime::command(crate::js_console::NODE_RUNTIME)
            .map_err(io::Error::other)?;
        let mut child = command.stderr(Stdio::inherit()).spawn()?;
        let mut stdin = child.stdin.take().expect("piped runtime stdin");
        let mut stdout = child.stdout.take().expect("piped runtime stdout");
        let (mut reader, mut writer) = socket.into_split();
        let input = tokio::io::copy(&mut reader, &mut stdin);
        let output = tokio::io::copy(&mut stdout, &mut writer);
        tokio::select! {
            _ = input => {}
            _ = output => {}
            _ = child.wait() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::path::PathBuf;

    #[test]
    fn staged_worker_reuses_matching_build_and_keeps_running_sessions_pinned() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let cache = dir.path().join("cache");
        let cache_path = quote(cache.clone().into_os_string()).unwrap();
        let stage = |content: &[u8], expect_upload: bool| {
            let digest = executable_digest(content).unwrap();
            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg(stage_script(content.len(), &digest, &cache_path))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let mut stdout = io::BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            let upload = line.trim_end() == format!("wayland-mcp upload {digest}");
            if upload {
                stdin.write_all(content).unwrap();
                line.clear();
                stdout.read_line(&mut line).unwrap();
            }
            let path = PathBuf::from(line.trim_end());
            assert_eq!(std::fs::read(&path).unwrap(), content);
            assert_eq!(upload, expect_upload);
            (child, stdin, path)
        };
        let first = stage(b"first", true);
        let hit = stage(b"first", false);
        let refresh = stage(b"other", true);
        assert_eq!(std::fs::read(&first.2).unwrap(), b"first");
        assert_eq!(std::fs::read(&hit.2).unwrap(), b"first");
        assert_eq!(std::fs::read(cache.join("wayland-mcp")).unwrap(), b"other");

        let damaged = dir.path().join("damaged");
        std::fs::write(&damaged, b"wrong").unwrap();
        std::fs::set_permissions(&damaged, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::rename(damaged, cache.join("wayland-mcp")).unwrap();
        let recovered = stage(b"other", true);
        assert_eq!(std::fs::read(&refresh.2).unwrap(), b"other");
        for (mut child, lease, path) in [first, hit, refresh, recovered] {
            drop(lease);
            assert!(child.wait().unwrap().success());
            assert!(!path.exists());
        }
    }
}
