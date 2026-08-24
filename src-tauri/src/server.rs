//! ServerManager: owns the `npx @deepseek-ai/dsh web` child process and
//! watches the port so the shell page can load it in an iframe as soon as
//! it is ready.

use serde::Serialize;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};

#[cfg(windows)]
mod windows_job {
    //! A Windows Job Object with KILL_ON_JOB_CLOSE: when the owning handle is
    //! closed (even if the app is force-killed and Drop code never runs), the
    //! OS terminates every process in the job — including the whole
    //! cmd → npx → node tree spawned for the dsh server.
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    pub struct KillOnCloseJob(HANDLE);

    // HANDLE is a raw pointer, so the wrapper is not automatically Send/Sync;
    // the handle is owned (closed in Drop) and its Win32 usage is thread-safe.
    unsafe impl Send for KillOnCloseJob {}
    unsafe impl Sync for KillOnCloseJob {}

    impl KillOnCloseJob {
        pub fn create() -> std::io::Result<Self> {
            unsafe {
                let job = CreateJobObjectW(None, None)
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    size,
                )
                .map_err(|e| std::io::Error::other(e.to_string()))?;
                Ok(Self(job))
            }
        }

        /// Attach a freshly spawned process (and, transitively, all of its
        /// descendants) to the job.
        pub fn assign(&self, pid: u32) -> std::io::Result<()> {
            unsafe {
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid)
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                let result = AssignProcessToJobObject(self.0, process);
                let _ = CloseHandle(process);
                result.map_err(|e| std::io::Error::other(e.to_string()))
            }
        }
    }

    impl Drop for KillOnCloseJob {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

#[cfg(windows)]
use windows_job::KillOnCloseJob;

pub const HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 3080;
pub const START_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum ServerState {
    Idle,
    Starting,
    Running,
    Error,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusPayload {
    pub state: ServerState,
    pub port: u16,
    pub url: String,
    pub error: Option<String>,
    pub elapsed_secs: Option<u64>,
}

pub struct ServerManager {
    port: u16,
    child: Option<Child>,
    #[cfg(windows)]
    job: Option<KillOnCloseJob>,
    state: ServerState,
    error: Option<String>,
    started_at: Option<Instant>,
    log_path: Option<PathBuf>,
    stopping: Arc<AtomicBool>,
}

impl ServerManager {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            child: None,
            #[cfg(windows)]
            job: KillOnCloseJob::create()
                .inspect_err(|e| eprintln!("[dsh] job create failed: {e}"))
                .ok(),
            state: ServerState::Idle,
            error: None,
            started_at: None,
            log_path: None,
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{HOST}:{}", self.port)
    }

    pub fn status(&self) -> StatusPayload {
        // If a foreign process already serves the harness on this port we
        // count as running (adopted), even though we did not spawn it.
        let adopted = self.child.is_none() && probe_harness(self.port);
        let state = if adopted {
            ServerState::Running
        } else {
            self.state
        };
        let elapsed = self.started_at.map(|t| t.elapsed().as_secs());
        StatusPayload {
            state,
            port: self.port,
            url: self.url(),
            error: self.error.clone(),
            elapsed_secs: elapsed,
        }
    }

    /// Start the dsh server (idempotent). Adopts an already-running harness
    /// on this port, otherwise spawns `npx @deepseek-ai/dsh web`.
    pub fn start(&mut self, app: &AppHandle) -> Result<(), String> {
        if self.state != ServerState::Idle && self.state != ServerState::Error {
            return Ok(());
        }
        self.error = None;

        if probe_harness(self.port) {
            self.state = ServerState::Running;
            self.started_at = Some(Instant::now());
            emit_status(app, &self.status());
            return Ok(());
        }

        if port_taken(self.port) {
            let msg = format!("端口 {} 已被其他程序占用，请关闭占用程序后重试", self.port);
            self.state = ServerState::Error;
            self.error = Some(msg.clone());
            emit_status(app, &self.status());
            return Err(msg);
        }

        let log_dir = app
            .path()
            .app_log_dir()
            .map_err(|e| format!("无法获取日志目录: {e}"))?;
        std::fs::create_dir_all(&log_dir)
            .map_err(|e| format!("无法创建日志目录: {e}"))?;
        let log_path = log_dir.join("dsh-server.log");
        let log_file = std::fs::File::create(&log_path)
            .map_err(|e| format!("无法创建日志文件: {e}"))?;

        let cmd_line = format!(
            "npx --yes @deepseek-ai/dsh web --host {HOST} --port {} --no-open",
            self.port
        );
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", &cmd_line])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file.try_clone().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(log_file));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: run the dsh process tree without a console
            // window (0x0800_0000).
            cmd.creation_flags(0x0800_0000);
        }
        let child = cmd
            .spawn()
            .map_err(|e| format!("无法启动 dsh 服务: {e}"))?;
        #[cfg(windows)]
        {
            // Attach the whole cmd → npx → node tree to a job object with
            // KILL_ON_JOB_CLOSE so it is guaranteed to die with the app.
            if let Some(job) = &self.job {
                job.assign(child.id())
                    .inspect_err(|e| eprintln!("[dsh] job assign failed: {e}"))
                    .ok();
            }
        }

        self.child = Some(child);
        self.state = ServerState::Starting;
        self.started_at = Some(Instant::now());
        self.log_path = Some(log_path);
        self.stopping.store(false, Ordering::SeqCst);
        emit_status(app, &self.status());

        self.spawn_watcher(app.clone());
        Ok(())
    }

    /// Stop the process we spawned. A harness that was merely adopted
    /// (spawned by something else) is left untouched.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            self.stopping.store(true, Ordering::SeqCst);
            #[cfg(windows)]
            {
                let _ = Command::new("taskkill")
                    .args(["/PID", &child.id().to_string(), "/T", "/F"])
                    .status();
                let _ = child.wait();
            }
            #[cfg(not(windows))]
            {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        self.state = ServerState::Idle;
        self.started_at = None;
    }

    /// Stop and start again.
    pub fn restart(&mut self, app: &AppHandle) -> Result<(), String> {
        self.stop();
        self.start(app)
    }

    /// Background thread: polls the port and emits status events; the shell
    /// page reacts by pointing the iframe at the app when it is ready.
    fn spawn_watcher(&self, app: AppHandle) {
        let port = self.port;
        let url = self.url();
        let stopping = self.stopping.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + START_TIMEOUT;
            let mut ready = false;
            loop {
                if stopping.load(Ordering::SeqCst) {
                    return;
                }
                let up = probe_port(port);
                if !ready {
                    if up {
                        ready = true;
                        if let Some(state) = app.try_state::<crate::AppState>() {
                            let mut mgr = state.server.lock().unwrap_or_else(|e| e.into_inner());
                            mgr.state = ServerState::Running;
                            mgr.error = None;
                            emit_status(&app, &mgr.status());
                        } else {
                            emit_status(&app, &StatusPayload {
                                state: ServerState::Running,
                                port,
                                url: url.clone(),
                                error: None,
                                elapsed_secs: None,
                            });
                        }
                    } else if Instant::now() > deadline {
                        if let Some(state) = app.try_state::<crate::AppState>() {
                            let mut mgr = state.server.lock().unwrap_or_else(|e| e.into_inner());
                            mgr.state = ServerState::Error;
                            mgr.error = Some(
                                "启动超时（300 秒）。请确认 Node.js 可用，并查看日志。".into(),
                            );
                            emit_status(&app, &mgr.status());
                        }
                        return;
                    }
                } else if !up {
                    if let Some(state) = app.try_state::<crate::AppState>() {
                        let mut mgr = state.server.lock().unwrap_or_else(|e| e.into_inner());
                        mgr.state = ServerState::Error;
                        mgr.error = Some("服务已停止或崩溃，请查看日志或重启服务。".into());
                        emit_status(&app, &mgr.status());
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
    }

    /// Tail of the server log, for diagnostics.
    pub fn tail_log(&self, max_lines: usize) -> String {
        let Some(path) = &self.log_path else {
            return String::new();
        };
        let Ok(content) = std::fs::read_to_string(path) else {
            return String::new();
        };
        content.lines().rev().take(max_lines).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
    }
}

impl Drop for ServerManager {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            #[cfg(windows)]
            {
                let _ = Command::new("taskkill")
                    .args(["/PID", &child.id().to_string(), "/T", "/F"])
                    .status();
                let _ = child.wait();
            }
            #[cfg(not(windows))]
            {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        // The job object (KILL_ON_JOB_CLOSE) is closed here; even if the
        // taskkill above missed any orphaned descendants, the OS terminates
        // everything left in the job when the handle goes away.
    }
}

pub fn emit_status(app: &AppHandle, payload: &StatusPayload) {
    let _ = app.emit("server-status", payload);
}

fn port_taken(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_ok()
}

fn probe_port(port: u16) -> bool {
    port_taken(port)
}

/// True when the port serves the DeepSeek Harness web app.
fn probe_harness(port: u16) -> bool {
    let mut stream = match TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    ) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));
    let _ = stream.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    let mut buf = [0u8; 8192];
    let mut total = Vec::new();
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => total.extend_from_slice(&buf[..n]),
        }
    }
    let head = String::from_utf8_lossy(&total);
    head.contains("DeepSeek Harness") || head.contains("__DSH_BOOT__")
}
