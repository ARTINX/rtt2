//! Local, per-user IPC for the shared session. No probe access lives here.
use serde::{Serialize, de::DeserializeOwned};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

const MAX_MESSAGE: usize = 8 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(15);

fn read_json<T: DeserializeOwned>(file: &mut std::fs::File) -> Result<T, String> {
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match file.read(&mut buffer) {
            // Windows std::fs::File may map ERROR_NO_DATA on a NOWAIT pipe
            // to a zero-byte read. It does not mean EOF while awaiting reply.
            Ok(0) =>
            {
                #[cfg(windows)]
                if !platform::alive(file) {
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
            Err(e) if matches!(e.raw_os_error(), Some(232 | 536)) => {}
            Err(e) => return Err(e.to_string()),
        }
        if Instant::now() >= deadline {
            return Err("RTT2 后端响应超时；已提交命令的执行结果可能未知，请勿自动重发".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn write_json(file: &mut impl Write, value: &impl Serialize) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    if bytes.len() > MAX_MESSAGE {
        return Err("RTT2 IPC 消息过长".into());
    }
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut offset = 0;
    while offset < bytes.len() {
        // NOWAIT writes larger than the pipe buffer can make no progress.
        // Bounded chunks also let the peer drain large telemetry batches.
        let end = (offset + 8192).min(bytes.len());
        match file.write(&bytes[offset..end]) {
            Ok(0) => {}
            Ok(n) => offset += n,
            Err(e) if matches!(e.raw_os_error(), Some(232 | 536)) => {}
            Err(e) => return Err(e.to_string()),
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

    fn create(first: bool) -> Result<File, String> {
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
        pub fn accept(&mut self) -> Result<Option<File>, String> {
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

#[cfg(windows)]
pub use platform::{Listener, request};

#[cfg(not(windows))]
pub fn request<Q: Serialize, R: DeserializeOwned>(_request: &Q) -> Result<R, String> {
    Err("RTT2 会话仅支持 Windows".into())
}

#[cfg(windows)]
pub fn serve_client<Q: DeserializeOwned, R: Serialize>(
    mut file: std::fs::File,
    handler: impl FnOnce(Q) -> R,
) {
    if let Ok(request) = read_json::<Q>(&mut file) {
        let _ = write_json(&mut file, &handler(request));
        // Keep the handle alive until the client consumes the reply. Flushing
        // a nonblocking named pipe is otherwise an unbounded blocking call.
        // The client closes immediately after its read; polling liveness is
        // bounded and prevents CloseHandle from discarding unread reply data.
        let deadline = Instant::now() + IO_TIMEOUT;
        while Instant::now() < deadline {
            if !platform::alive(&file) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn pipe_reserves_one_worker_and_transfers_large_unicode_messages() {
        let mut listener = Listener::bind().unwrap();
        assert!(Listener::bind().is_err());
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(file) = listener.accept().unwrap() {
                    serve_client(file, |input: String| input);
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
