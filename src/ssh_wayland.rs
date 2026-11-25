use crate::gui_backend_wayland::bridge::{self, End, Endpoint, VideoSettings};
use crate::gui_color::ColorDescription;
use crate::ssh_transport::{quote, stage_executable};
use crate::ssh_video;
use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::net::Shutdown;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::sync::oneshot;

pub(crate) const VIDEO_SOCKET_ENV: &str = "WAYLAND_MCP_SSH_VIDEO_SOCKET";
const MAX_CONFIG_LINE: u64 = 16 * 1024;

#[derive(Default)]
struct LaunchOptions {
    destination: OsString,
    ssh_config: Option<OsString>,
    local_display: Option<OsString>,
    local_control: Option<PathBuf>,
    remote_gpu: OsString,
    display_gpu: Option<String>,
    shell: Vec<OsString>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) struct ApplicationEnvironment {
    wayland_display: PathBuf,
    wayland_mcp_backend_socket: PathBuf,
    wayland_mcp_ssh_video_socket: PathBuf,
}

struct ConsoleReady {
    environment: ApplicationEnvironment,
    video_control: PathBuf,
}

pub(crate) struct ConsoleBridge {
    task: tokio::task::JoinHandle<Result<(), String>>,
    video_control: PathBuf,
}

impl Drop for ConsoleBridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ConsoleBridge {
    pub(crate) async fn connect(
        host: &str,
        display: String,
    ) -> Result<(Self, ApplicationEnvironment), String> {
        let login_part = |part: &str| {
            !part.is_empty()
                && !part.starts_with('-')
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        };
        let (user, address) = host
            .split_once('@')
            .map_or((None, host), |(user, address)| (Some(user), address));
        let ipv6 = address
            .strip_prefix('[')
            .and_then(|address| address.strip_suffix(']'))
            .unwrap_or(address);
        if host.len() > 255
            || user.is_some_and(|user| !login_part(user))
            || !(login_part(address) || ipv6.parse::<std::net::Ipv6Addr>().is_ok())
        {
            return Err("connectSsh requires an SSH hostname or user@host".into());
        }
        let (sender, ready) = oneshot::channel();
        let options = LaunchOptions {
            destination: host.into(),
            local_display: Some(display.into()),
            ..Default::default()
        };
        let mut session = Self {
            task: tokio::spawn(launch(options, Some(sender))),
            video_control: PathBuf::new(),
        };
        let ready = tokio::select! {
            result = ready => match result {
                Ok(ready) => ready,
                Err(_) => return Err(bridge_ended((&mut session.task).await)),
            },
            result = &mut session.task => return Err(bridge_ended(result)),
            _ = tokio::time::sleep(Duration::from_secs(60)) => return Err("SSH Wayland connection timed out".into()),
        };
        session.video_control = ready.video_control;
        Ok((session, ready.environment))
    }

    pub(crate) async fn configure(
        &self,
        config: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        if self.task.is_finished() {
            return Err("SSH Wayland connection has closed".into());
        }
        configure_at(&self.video_control, config).await
    }
}

fn bridge_ended(result: Result<Result<(), String>, tokio::task::JoinError>) -> String {
    match result {
        Ok(Ok(())) => "SSH Wayland connection closed before verification".into(),
        Ok(Err(error)) => error,
        Err(error) => format!("SSH Wayland launcher failed: {error}"),
    }
}

fn default_application_locale() -> Option<&'static str> {
    if ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
    {
        return None;
    }
    let locale = unsafe {
        libc::newlocale(
            libc::LC_CTYPE_MASK,
            c"C.UTF-8".as_ptr(),
            std::ptr::null_mut(),
        )
    };
    if locale.is_null() {
        return None;
    }
    unsafe { libc::freelocale(locale) };
    Some("C.UTF-8")
}

pub(crate) async fn run(mut args: impl Iterator<Item = OsString>) -> Result<(), String> {
    let mut options = LaunchOptions {
        destination: args.next().ok_or("--ssh-wayland requires a destination")?,
        ..Default::default()
    };
    while let Some(flag) = args.next() {
        if flag == "--" {
            options.shell.extend(args);
            break;
        }
        let value = args
            .next()
            .ok_or_else(|| format!("{} requires a value", flag.to_string_lossy()))?;
        match flag.to_str() {
            Some("--ssh-config") => options.ssh_config = Some(value),
            Some("--local-display") => options.local_display = Some(value),
            Some("--local-control") => options.local_control = Some(PathBuf::from(value)),
            Some("--remote-gpu") => options.remote_gpu = value,
            Some("--display-gpu") => options.display_gpu = Some(string(value)?),
            _ => {
                return Err(format!(
                    "unknown SSH Wayland option {}",
                    flag.to_string_lossy()
                ));
            }
        }
    }
    launch(options, None).await
}

async fn launch(
    options: LaunchOptions,
    ready: Option<oneshot::Sender<ConsoleReady>>,
) -> Result<(), String> {
    let LaunchOptions {
        destination,
        ssh_config,
        local_display,
        local_control,
        remote_gpu,
        display_gpu,
        shell,
    } = options;
    let console = ready.is_some();
    let display = resolve_display(local_display)?;
    let prepare_decoder = async {
        let decoders = tokio::task::spawn_blocking(ssh_video::local_decoders)
            .await
            .map_err(error)?;
        let default_gpu = decoders
            .keys()
            .next()
            .ok_or("no local GPU has a supported H.265 decoder")?;
        let display_gpu = display_gpu.unwrap_or_else(|| default_gpu.clone());
        ssh_video::render_affinity(&display_gpu)?;
        let endpoint = Arc::new(Endpoint {
            end: End::Compositor,
            promote_shm: console,
            warmup: ssh_video::ConverterWarmup::start(&display_gpu),
            gpu: Some(display_gpu),
            video: default_video()?,
        });
        Ok::<_, String>((decoders, endpoint))
    };
    let ((decoders, endpoint), worker) = tokio::try_join!(
        prepare_decoder,
        stage_executable(&destination, ssh_config.as_deref(), console),
    )?;
    let directory = tempfile::Builder::new()
        .prefix("wmc-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")
        .map_err(error)?;
    let tunnel_path = directory.path().join("wire");
    let local = UnixListener::bind(&tunnel_path).map_err(error)?;
    let lifetime_path = directory.path().join("lifetime");
    let lifetime_listener = UnixListener::bind(&lifetime_path).map_err(error)?;
    let private_control = directory.path().join("video");
    let control_alias = local_control
        .map(|path| {
            if !path.is_absolute() {
                return Err("--local-control requires an absolute path".to_owned());
            }
            std::os::unix::fs::symlink(&private_control, &path)
                .map_err(|err| format!("create video control alias {}: {err}", path.display()))?;
            Ok(SocketPath(path))
        })
        .transpose()?;
    let control = control_alias
        .as_ref()
        .map_or(&private_control, |alias| &alias.0);
    let name = directory.path().file_name().unwrap().to_string_lossy();
    let remote_tunnel = format!("/tmp/{name}-wire");
    let remote_lifetime = format!("/tmp/{name}-lifetime");
    let remote_control = format!("/tmp/{name}-video");
    let remote_args = [
        "--ssh-wayland-worker".into(),
        name.as_ref().into(),
        remote_gpu,
    ]
    .into_iter()
    .chain(console.then(|| "--console-ready".into()))
    .chain(["--".into()])
    .chain(shell.iter().cloned())
    .map(shell_arg)
    .collect::<Result<Vec<_>, _>>()?
    .join(" ");
    let capabilities = quote(serde_json::to_string(decoders).map_err(error)?.into())?;
    let timing = if ssh_video::timing_enabled() {
        "export WAYLAND_MCP_TRACE_TIMING=1\n"
    } else {
        ""
    };
    let remote = format!(
        "{}{timing}export {}={capabilities}\nexec \"$WAYLAND_MCP_EXECUTABLE\" {remote_args}",
        worker.environment(),
        ssh_video::SSH_DECODERS_ENV,
    );

    let mut ssh = Command::new("ssh");
    if let Some(config) = ssh_config {
        ssh.arg("-F").arg(config);
    }
    let have_terminal = !shell.is_empty() && io::stdin().is_terminal();
    if console {
        ssh.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
    }
    ssh.arg(if have_terminal { "-t" } else { "-T" })
        .args([
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "StreamLocalBindUnlink=no",
            "-o",
            "StreamLocalBindMask=0177",
            "-o",
            "ControlPath=none",
        ])
        .arg("-R")
        .arg(format!("{remote_tunnel}:{}", tunnel_path.display()))
        .arg("-R")
        .arg(format!("{remote_lifetime}:{}", lifetime_path.display()))
        .arg("-L")
        .arg(format!("{}:{remote_control}", private_control.display()))
        .arg("--")
        .arg(destination)
        .arg(remote)
        .stdin(if console {
            Stdio::null()
        } else {
            Stdio::inherit()
        })
        .stdout(if console {
            Stdio::null()
        } else {
            Stdio::inherit()
        })
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut ssh = ssh.spawn().map_err(error)?;
    eprintln!("SSH Wayland local compositor socket {}", display.display());
    eprintln!(
        "SSH Wayland local video control socket {}",
        control.display()
    );
    let mut connections = Connections::default();
    let lifetime = async {
        let (mut stream, _) = lifetime_listener.accept().await.map_err(error)?;
        if let Some(sender) = ready {
            let mut report = Vec::new();
            BufReader::new((&mut stream).take(MAX_CONFIG_LINE))
                .read_until(b'\n', &mut report)
                .await
                .map_err(error)?;
            if report.last() != Some(&b'\n') {
                return Err("incomplete SSH Wayland verification response".into());
            }
            let environment = serde_json::from_slice(&report).map_err(error)?;
            sender
                .send(ConsoleReady {
                    environment,
                    video_control: control.clone(),
                })
                .map_err(|_| "SSH Wayland connection was canceled")?;
        }
        wait_for_lifetime_close(stream).await
    };
    tokio::pin!(lifetime);
    let mut lifetime_open = true;
    loop {
        tokio::select! {
            status = ssh.wait() => return status_message(status.map_err(error)?),
            result = &mut lifetime, if lifetime_open => {
                result?;
                lifetime_open = false;
            }
            accepted = local.accept() => {
                let (stream, _) = accepted.map_err(error)?;
                connections.start(stream, display.clone(), Arc::clone(&endpoint))?;
            }
        }
    }
}

pub(crate) async fn worker(mut args: impl Iterator<Item = OsString>) -> Result<(), String> {
    ssh_video::enable_ssh_worker()?;
    let name = string(args.next().ok_or("missing SSH Wayland session name")?)?;
    if !name.strip_prefix("wmc-").is_some_and(|name| {
        name.len() == 6 && name.bytes().all(|byte| byte.is_ascii_alphanumeric())
    }) {
        return Err("invalid SSH Wayland session name".into());
    }
    let tunnel = PathBuf::from(format!("/tmp/{name}-wire"));
    let control_path = PathBuf::from(format!("/tmp/{name}-video"));
    let lifetime_path = PathBuf::from(format!("/tmp/{name}-lifetime"));
    let _tunnel_cleanup = SocketPath(tunnel.clone());
    let _lifetime_cleanup = SocketPath(lifetime_path.clone());
    let lifetime = UnixStream::connect(&lifetime_path)
        .await
        .map_err(|error| format!("connect SSH Wayland lifetime socket: {error}"))?;
    let gpu = string(
        args.next()
            .ok_or("missing SSH Wayland render node argument")?,
    )?;
    let separator = args.next();
    let report_ready = separator.as_deref() == Some(std::ffi::OsStr::new("--console-ready"));
    let separator = if report_ready { args.next() } else { separator };
    if separator.as_deref() != Some(std::ffi::OsStr::new("--")) {
        return Err("missing SSH Wayland command separator".into());
    }
    let video = default_video()?;
    let settings = video.read().map_err(error)?.clone();
    let gpu = tokio::task::spawn_blocking(move || {
        ssh_video::bridge_gpu(
            &gpu,
            &settings.request,
            settings.rate_control,
            settings.preserve_alpha,
        )
    })
    .await
    .map_err(error)??;
    let endpoint = Arc::new(Endpoint {
        end: End::Application,
        promote_shm: false,
        warmup: ssh_video::ConverterWarmup::start(&gpu),
        gpu: Some(gpu.clone()),
        video: Arc::clone(&video),
    });
    let directory = tempfile::Builder::new()
        .prefix("wmc-app-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")
        .map_err(error)?;
    let display = directory.path().join("wayland");
    let listener = UnixListener::bind(&display).map_err(error)?;
    // Bind the SSH forwarding target inside our private directory. A symlink
    // gives OpenSSH the short, per-session path supplied by the launcher.
    let private_control = directory.path().join("video");
    let controls = UnixListener::bind(&private_control).map_err(error)?;
    std::os::unix::fs::symlink(&private_control, &control_path).map_err(error)?;
    let _control_cleanup = SocketPath(control_path.clone());
    let command = args.next();
    let (lifetime, mut announce) = lifetime.into_split();
    let session = async {
        if report_ready {
            let path = display.clone();
            tokio::task::spawn_blocking(move || {
                crate::gui_backend_wayland::probe_compositor(&path)
            })
            .await
            .map_err(error)??;
            let mut report = serde_json::to_vec(&ApplicationEnvironment {
                wayland_display: display.clone(),
                wayland_mcp_backend_socket: display,
                wayland_mcp_ssh_video_socket: private_control,
            })
            .map_err(error)?;
            report.push(b'\n');
            announce.write_all(&report).await.map_err(error)?;
            std::future::pending().await
        } else if let Some(command) = command {
            let mut child = Command::new(command);
            child
                .args(args)
                .env("WAYLAND_DISPLAY", &display)
                .env_remove("WAYLAND_SOCKET")
                .env("WAYLAND_MCP_BACKEND_SOCKET", &display)
                .env(VIDEO_SOCKET_ENV, &private_control)
                .kill_on_drop(true);
            if let Some(locale) = default_application_locale() {
                child.env("LANG", locale);
            }
            let mut child = child.spawn().map_err(error)?;
            status_message(child.wait().await.map_err(error)?)
        } else {
            eprintln!("SSH Wayland application environment");
            println!(
                "export WAYLAND_DISPLAY={} WAYLAND_MCP_BACKEND_SOCKET={} {}={}",
                quote(display.as_os_str().into())?,
                quote(display.as_os_str().into())?,
                VIDEO_SOCKET_ENV,
                quote(private_control.as_os_str().into())?
            );
            let (send, receive) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let result = io::copy(&mut io::stdin().lock(), &mut io::sink()).map_err(error);
                let _ = send.send(result);
            });
            receive.await.map_err(error)??;
            Ok(())
        }
    };
    let lifetime = wait_for_lifetime_close(lifetime);
    tokio::pin!(session, lifetime);
    let mut connections = Connections::default();
    loop {
        tokio::select! {
            result = &mut session => return result,
            result = &mut lifetime => return result,
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(error)?;
                connections.start(stream, tunnel.clone(), Arc::clone(&endpoint))?;
            }
            accepted = controls.accept() => {
                let (stream, _) = accepted.map_err(error)?;
                let config = Arc::clone(&video);
                let gpu = gpu.clone();
                tokio::spawn(async move {
                    if let Err(err) = accept_config(stream, config, gpu).await {
                        eprintln!("SSH Wayland video control: {err}");
                    }
                });
            }
        }
    }
}

async fn wait_for_lifetime_close(mut stream: impl AsyncReadExt + Unpin) -> Result<(), String> {
    match stream.read_u8().await {
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error.to_string()),
        Ok(_) => Err("unexpected data on SSH Wayland lifetime socket".into()),
    }
}

struct Connection {
    socket: Arc<StdUnixStream>,
    thread: std::thread::JoinHandle<()>,
}

#[derive(Default)]
struct Connections(Vec<Connection>);

impl Connections {
    fn start(
        &mut self,
        socket: UnixStream,
        connect: PathBuf,
        endpoint: Arc<Endpoint>,
    ) -> Result<(), String> {
        while let Some(index) = self
            .0
            .iter()
            .position(|connection| connection.thread.is_finished())
        {
            let _ = self.0.swap_remove(index).thread.join();
        }
        let socket = Arc::new(socket.into_std().map_err(error)?);
        socket.set_nonblocking(false).map_err(error)?;
        let accepted = Arc::clone(&socket);
        let thread = std::thread::Builder::new()
            .name("wayland-ssh".into())
            .spawn(move || {
                let result = StdUnixStream::connect(&connect)
                    .map_err(error)
                    .and_then(|peer| {
                        if endpoint.end == End::Compositor {
                            bridge::relay(&peer, &accepted, &endpoint)
                        } else {
                            bridge::relay(&accepted, &peer, &endpoint)
                        }
                    });
                let _ = accepted.shutdown(Shutdown::Both);
                if let Err(err) = result {
                    eprintln!("SSH Wayland connection: {err}");
                }
            })
            .map_err(error)?;
        self.0.push(Connection { socket, thread });
        Ok(())
    }
}

impl Drop for Connections {
    fn drop(&mut self) {
        for connection in &self.0 {
            let _ = connection.socket.shutdown(Shutdown::Both);
        }
        // The relay threads own Vulkan and codec contexts. Let their native
        // thread-local destructors finish before the driver unloads.
        for connection in self.0.drain(..) {
            let _ = connection.thread.join();
        }
    }
}

fn resolve_display(display: Option<OsString>) -> Result<PathBuf, String> {
    let display = display
        .or_else(|| std::env::var_os("WAYLAND_DISPLAY"))
        .ok_or("set WAYLAND_DISPLAY or pass --local-display")?;
    let path = PathBuf::from(display);
    let path = if path.is_absolute() {
        path
    } else {
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?)
            .join(path)
    };
    if !std::fs::metadata(&path).is_ok_and(|metadata| metadata.file_type().is_socket()) {
        return Err(format!(
            "local Wayland socket {} does not exist",
            path.display()
        ));
    }
    Ok(path)
}

fn string(value: OsString) -> Result<String, String> {
    value
        .into_string()
        .map_err(|_| "SSH Wayland options require UTF-8".into())
}

fn shell_arg(value: OsString) -> Result<String, String> {
    if value.is_empty() {
        return Ok("''".into());
    }
    quote(value)
}

fn error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn status_message(status: std::process::ExitStatus) -> Result<(), String> {
    if status.success() {
        Ok(())
    } else {
        Err(format!("SSH Wayland session exited with {status}"))
    }
}

struct SocketPath(PathBuf);
impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn default_video() -> Result<Arc<RwLock<VideoSettings>>, String> {
    Ok(Arc::new(RwLock::new(parse_config(serde_json::json!({}))?)))
}

fn parse_config(mut value: serde_json::Value) -> Result<VideoSettings, String> {
    let fields = value
        .as_object_mut()
        .ok_or("SSH Wayland video options must be an object")?;
    let source_color = fields
        .remove("sourceColor")
        .as_ref()
        .map(ColorDescription::visual_assumption)
        .transpose()?;
    if fields.contains_key("qp") {
        return Err("SSH Wayland uses rateControl with vbr or constantQuality".into());
    }
    let rate_control = fields
        .remove("rateControl")
        .map(serde_json::from_value)
        .transpose()
        .map_err(error)?
        .unwrap_or_default();
    let preserve_alpha = match fields.get("background") {
        None => true,
        Some(background) if background == "transparent" => {
            fields.remove("background");
            true
        }
        Some(_) => false,
    };
    fields.insert("windowId".into(), "".into());
    Ok(VideoSettings {
        request: serde_json::from_value(value).map_err(error)?,
        source_color,
        preserve_alpha,
        rate_control,
    })
}

async fn accept_config(
    mut stream: UnixStream,
    video: Arc<RwLock<VideoSettings>>,
    gpu: String,
) -> Result<(), String> {
    let mut line = Vec::new();
    BufReader::new((&mut stream).take(MAX_CONFIG_LINE))
        .read_until(b'\n', &mut line)
        .await
        .map_err(error)?;
    let result = tokio::task::spawn_blocking(move || {
        if line.last() != Some(&b'\n') {
            return Err("incomplete SSH Wayland configuration".to_owned());
        }
        let config = parse_config(serde_json::from_slice(&line).map_err(error)?)?;
        let mut selected = ssh_video::bridge_config(
            &config.request,
            &gpu,
            config.rate_control,
            config.preserve_alpha,
        )?;
        selected["background"] = if config.preserve_alpha {
            "transparent".into()
        } else {
            serde_json::to_value(config.request.background).map_err(error)?
        };
        *video.write().map_err(error)? = config;
        Ok(selected)
    })
    .await
    .map_err(error)?;
    let response = match result {
        Ok(selected) => selected,
        Err(error) => serde_json::json!({"error": error}),
    };
    let mut response = serde_json::to_vec(&response).map_err(error)?;
    response.push(b'\n');
    stream.write_all(&response).await.map_err(error)
}

pub(crate) async fn configure(config: serde_json::Value) -> Result<serde_json::Value, String> {
    let socket = std::env::var_os(VIDEO_SOCKET_ENV)
        .ok_or("SSH Wayland video control socket is not configured")?;
    configure_at(Path::new(&socket), config).await
}

async fn configure_at(
    socket: &Path,
    config: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut data = serde_json::to_vec(&config).map_err(error)?;
    if data.len() as u64 >= MAX_CONFIG_LINE {
        return Err("SSH Wayland video configuration is too large".into());
    }
    data.push(b'\n');
    let mut attempts = 0;
    let line = loop {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(4),
            exchange_config(socket, &data),
        )
        .await
        .map_err(|_| "SSH Wayland video control timed out")?;
        match result {
            Ok(line) => break line,
            Err(err)
                if attempts < 20
                    && matches!(
                        err.kind(),
                        io::ErrorKind::ConnectionReset
                            | io::ErrorKind::BrokenPipe
                            | io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::NotFound
                            | io::ErrorKind::ConnectionRefused
                    ) =>
            {
                attempts += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(err) => return Err(error(err)),
        }
    };
    let response: serde_json::Value = serde_json::from_slice(&line).map_err(error)?;
    if let Some(error) = response.get("error").and_then(serde_json::Value::as_str) {
        return Err(error.to_owned());
    }
    Ok(response)
}

async fn exchange_config(socket: &Path, data: &[u8]) -> io::Result<Vec<u8>> {
    let mut stream = UnixStream::connect(socket).await?;
    stream.write_all(data).await?;
    let mut line = Vec::new();
    BufReader::new(stream.take(MAX_CONFIG_LINE))
        .read_until(b'\n', &mut line)
        .await?;
    if line.last() != Some(&b'\n') {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(line)
}
