//! Local, per-user IPC for the shared session. No probe access lives here.
use serde::{Serialize, de::DeserializeOwned};
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

const MAX_MESSAGE: usize = 8 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(windows)]
pub type Stream = std::fs::File;
#[cfg(unix)]
pub type Stream = std::os::unix::net::UnixStream;

pub(crate) trait Connection: Read + Write {
    /// Whether a zero-length read means end-of-file rather than "no data yet".
    const ZERO_READ_IS_EOF: bool;

    fn alive(&self) -> bool;
    fn finish_write(&mut self);
}

fn retryable(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    ) {
        return true;
    }
    #[cfg(windows)]
    if matches!(error.raw_os_error(), Some(232 | 536)) {
        // ERROR_NO_DATA and ERROR_PIPE_LISTENING on a nonblocking pipe.
        return true;
    }
    false
}

fn read_json<S: Connection, T: DeserializeOwned>(stream: &mut S) -> Result<T, String> {
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => {
                // Windows std::fs::File may map ERROR_NO_DATA on a NOWAIT pipe
                // to a zero-byte read. It does not mean EOF while awaiting reply.
                if S::ZERO_READ_IS_EOF || !stream.alive() {
                    return Err("RTT2 后端连接已关闭".into());
                }
            }
            Ok(n) => {
                data.extend_from_slice(&buffer[..n]);
                if data.len() > MAX_MESSAGE {
                    return Err("RTT2 IPC 消息过长".into());
                }
                if let Some(end) = data.iter().position(|b| *b == b'\n') {
                    return serde_json::from_slice(&data[..end]).map_err(|e| e.to_string());
                }
            }
            Err(error) if retryable(&error) => {}
            Err(error) => return Err(error.to_string()),
        }
        if Instant::now() >= deadline {
            return Err("RTT2 后端响应超时；已提交命令的执行结果可能未知，请勿自动重发".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn write_json<S: Write>(stream: &mut S, value: &impl Serialize) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    if bytes.len() > MAX_MESSAGE {
        return Err("RTT2 IPC 消息过长".into());
    }
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut offset = 0;
    while offset < bytes.len() {
        // Partial writes are normal for nonblocking pipes and sockets. Bounded
        // chunks also let the peer drain large telemetry batches.
        let end = (offset + 8192).min(bytes.len());
        match stream.write(&bytes[offset..end]) {
            Ok(0) => {}
            Ok(n) => offset += n,
            Err(error) if retryable(&error) => {}
            Err(error) => return Err(error.to_string()),
        }
        if Instant::now() >= deadline {
            return Err("RTT2 IPC 写入超时".into());
        }
        if offset < bytes.len() {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    Ok(())
}

#[cfg(windows)]
impl Connection for std::fs::File {
    const ZERO_READ_IS_EOF: bool = false;

    fn alive(&self) -> bool {
        platform::alive(self)
    }

    fn finish_write(&mut self) {}
}

#[cfg(unix)]
impl Connection for std::os::unix::net::UnixStream {
    const ZERO_READ_IS_EOF: bool = true;

    fn alive(&self) -> bool {
        true
    }

    fn finish_write(&mut self) {
        let _ = self.shutdown(std::net::Shutdown::Write);
    }
}

pub(crate) fn serve_client<S, Q, R>(mut stream: S, handler: impl FnOnce(Q) -> R)
where
    S: Connection,
    Q: DeserializeOwned,
    R: Serialize,
{
    if let Ok(request) = read_json::<S, Q>(&mut stream) {
        let _ = write_json(&mut stream, &handler(request));
        stream.finish_write();
        // Keep the Windows handle alive until the client consumes the reply.
        // Closing a nonblocking named pipe can otherwise discard unread data.
        #[cfg(windows)]
        {
            let deadline = Instant::now() + IO_TIMEOUT;
            while Instant::now() < deadline {
                if !stream.alive() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use std::fs::File;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::ptr;

    type Handle = *mut c_void;
    const INVALID: Handle = -1isize as Handle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open: u32,
            mode: u32,
            instances: u32,
            out_size: u32,
            in_size: u32,
            timeout: u32,
            security: *mut c_void,
        ) -> Handle;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *mut c_void,
            creation: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn ConnectNamedPipe(pipe: Handle, overlapped: *mut c_void) -> i32;
        fn SetNamedPipeHandleState(
            pipe: Handle,
            mode: *const u32,
            count: *const u32,
            timeout: *const u32,
        ) -> i32;
        fn GetLastError() -> u32;
        fn PeekNamedPipe(
            pipe: Handle,
            buffer: *mut c_void,
            size: u32,
            read: *mut u32,
            available: *mut u32,
            left: *mut u32,
        ) -> i32;
    }

    fn name() -> Vec<u16> {
        let user = std::env::var("USERNAME").unwrap_or_default();
        let user: String = user
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let suffix = if cfg!(test) {
            format!("-test-{}", std::process::id())
        } else {
            String::new()
        };
        format!(r"\\.\pipe\rtt2-session-v130-{user}{suffix}")
            .encode_utf16()
            .chain(Some(0))
            .collect()
    }

    pub(super) fn alive(file: &File) -> bool {
        unsafe {
            PeekNamedPipe(
                file.as_raw_handle(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ) != 0
        }
    }

    fn create(first: bool) -> Result<Stream, String> {
        // First-instance reservation is held until the next instance is ready.
        // The default user DACL and REJECT_REMOTE_CLIENTS keep IPC local.
        let handle = unsafe {
            CreateNamedPipeW(
                name().as_ptr(),
                3 | if first { 0x0008_0000 } else { 0 },
                1 | 8,
                255,
                65536,
                65536,
                0,
                ptr::null_mut(),
            )
        };
        if handle == INVALID {
            return Err(format!("RTT2 后端已启动或管道创建失败：{}", unsafe {
                GetLastError()
            }));
        }
        Ok(unsafe { File::from_raw_handle(handle) })
    }

    pub struct Listener {
        next: File,
    }
    impl Listener {
        pub fn bind() -> Result<Self, String> {
            Ok(Self {
                next: create(true)?,
            })
        }
        pub fn accept(&mut self) -> Result<Option<Stream>, String> {
            let connected =
                unsafe { ConnectNamedPipe(self.next.as_raw_handle(), ptr::null_mut()) } != 0;
            let error = if connected {
                0
            } else {
                unsafe { GetLastError() }
            };
            if connected || error == 535 {
                let next = create(false)?;
                return Ok(Some(std::mem::replace(&mut self.next, next)));
            }
            if matches!(error, 232 | 536) {
                return Ok(None);
            }
            Err(format!("RTT2 管道接受连接失败：{error}"))
        }
    }

    pub fn request<Q: Serialize, R: DeserializeOwned>(request: &Q) -> Result<R, String> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut file = loop {
            let handle = unsafe {
                CreateFileW(
                    name().as_ptr(),
                    0xc000_0000,
                    0,
                    ptr::null_mut(),
                    3,
                    0,
                    ptr::null_mut(),
                )
            };
            if handle != INVALID {
                break unsafe { File::from_raw_handle(handle) };
            }
            let error = unsafe { GetLastError() };
            if error != 231 || Instant::now() >= deadline {
                return Err(format!("RTT2 后端未就绪（Win32 {error}）"));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let mode = 1u32; // PIPE_NOWAIT, byte mode; explicit bounded I/O below.
        if unsafe { SetNamedPipeHandleState(file.as_raw_handle(), &mode, ptr::null(), ptr::null()) }
            == 0
        {
            return Err(format!("无法设置 RTT2 管道模式：{}", unsafe {
                GetLastError()
            }));
        }
        write_json(&mut file, request)?;
        read_json(&mut file)
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::fs::{self, File, OpenOptions};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;

    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    const SOCKET_MODE: u32 = 0o600;
    const DIRECTORY_MODE: u32 = 0o700;
    const IO_POLL: Duration = Duration::from_millis(100);

    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
        fn geteuid() -> u32;
    }

    fn uid() -> u32 {
        unsafe { geteuid() }
    }

    fn suffix() -> String {
        if cfg!(test) {
            format!("-test-{}", std::process::id())
        } else {
            String::new()
        }
    }

    /// A deterministic, per-user location. It deliberately ignores
    /// XDG_RUNTIME_DIR/TMPDIR so a GUI launched by the desktop can attach to a
    /// worker started by a terminal with a different environment.
    fn runtime_dir() -> PathBuf {
        PathBuf::from("/tmp").join(format!("rtt2-{}", uid()))
    }

    fn socket_dir() -> Result<PathBuf, String> {
        let dir = runtime_dir();
        match fs::DirBuilder::new()
            .recursive(true)
            .mode(DIRECTORY_MODE)
            .create(&dir)
        {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "无法创建 RTT2 运行时目录 {}: {error}",
                    dir.display()
                ));
            }
        }
        let metadata = fs::symlink_metadata(&dir)
            .map_err(|e| format!("无法检查 RTT2 运行时目录 {}: {e}", dir.display()))?;
        if !metadata.is_dir() || metadata.uid() != uid() {
            return Err(format!(
                "RTT2 运行时目录 {} 不属于当前用户，拒绝使用",
                dir.display()
            ));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            fs::set_permissions(&dir, fs::Permissions::from_mode(DIRECTORY_MODE))
                .map_err(|e| format!("无法保护 RTT2 运行时目录 {}: {e}", dir.display()))?;
        }
        Ok(dir)
    }

    fn socket_path() -> Result<PathBuf, String> {
        Ok(socket_dir()?.join(format!("rtt2-session-v130-{}{}.sock", uid(), suffix())))
    }

    fn lock_path() -> Result<PathBuf, String> {
        Ok(socket_dir()?.join(format!("rtt2-session-v130-{}{}.lock", uid(), suffix())))
    }

    pub struct Listener {
        listener: UnixListener,
        path: PathBuf,
        _lock: File,
    }

    impl Listener {
        pub fn bind() -> Result<Self, String> {
            let path = socket_path()?;
            let lock_path = lock_path()?;
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
                .map_err(|e| format!("无法创建 RTT2 锁文件 {}: {e}", lock_path.display()))?;
            // The lock serializes stale-socket cleanup and bind across worker
            // starts. Drop releases it automatically.
            if unsafe { flock(lock.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
                return Err("RTT2 后端已启动".into());
            }
            if path.exists() {
                // A live listener means another worker owns the session. A
                // refused connection leaves a stale socket from a crash.
                if UnixStream::connect(&path).is_ok() {
                    return Err("RTT2 后端已启动".into());
                }
                fs::remove_file(&path)
                    .map_err(|e| format!("无法清理旧的 RTT2 套接字 {}: {e}", path.display()))?;
            }
            let listener = UnixListener::bind(&path)
                .map_err(|e| format!("RTT2 套接字创建失败 {}: {e}", path.display()))?;
            listener
                .set_nonblocking(true)
                .map_err(|e| format!("无法设置 RTT2 套接字模式：{e}"))?;
            fs::set_permissions(&path, fs::Permissions::from_mode(SOCKET_MODE))
                .map_err(|e| format!("无法保护 RTT2 套接字 {}: {e}", path.display()))?;
            Ok(Self {
                listener,
                path,
                _lock: lock,
            })
        }

        pub fn accept(&mut self) -> Result<Option<Stream>, String> {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_read_timeout(Some(IO_POLL))
                        .map_err(|e| format!("无法设置 RTT2 套接字读取超时：{e}"))?;
                    stream
                        .set_write_timeout(Some(IO_POLL))
                        .map_err(|e| format!("无法设置 RTT2 套接字写入超时：{e}"))?;
                    Ok(Some(stream))
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    Ok(None)
                }
                Err(error) => Err(format!("RTT2 套接字接受连接失败：{error}")),
            }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    pub fn request<Q: Serialize, R: DeserializeOwned>(request: &Q) -> Result<R, String> {
        let path = socket_path()?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut stream = loop {
            match UnixStream::connect(&path) {
                Ok(stream) => break stream,
                Err(error) => {
                    let retry = matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::Interrupted
                    );
                    if !retry || Instant::now() >= deadline {
                        return Err(if error.kind() == io::ErrorKind::NotFound {
                            "RTT2 后端未运行".into()
                        } else {
                            format!("RTT2 后端未就绪（{error}）")
                        });
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };
        stream
            .set_read_timeout(Some(IO_POLL))
            .map_err(|e| format!("无法设置 RTT2 套接字读取超时：{e}"))?;
        stream
            .set_write_timeout(Some(IO_POLL))
            .map_err(|e| format!("无法设置 RTT2 套接字写入超时：{e}"))?;
        write_json(&mut stream, request)?;
        read_json(&mut stream)
    }
}

#[cfg(any(windows, unix))]
pub use platform::{Listener, request};

#[cfg(not(any(windows, unix)))]
pub fn request<Q: Serialize, R: DeserializeOwned>(_request: &Q) -> Result<R, String> {
    Err("RTT2 会话不支持当前平台".into())
}

#[cfg(all(test, any(windows, unix)))]
mod tests {
    use super::*;

    #[test]
    fn listener_reserves_one_worker_and_transfers_large_unicode_messages() {
        let mut listener = Listener::bind().unwrap();
        assert!(Listener::bind().is_err());
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(stream) = listener.accept().unwrap() {
                    serve_client(stream, |input: String| input);
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let input = "遥测数据1234567890".repeat(16384);
        let output: String = request(&input).unwrap();
        assert_eq!(output, input);
        server.join().unwrap();
    }
}
