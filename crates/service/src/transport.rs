//! Local IPC transport for the JSON-RPC socket service (issue #581).
//!
//! Unix keeps today's Unix domain socket at the configured path (`LCG_SOCKET_PATH`, default
//! `.lcg/service.sock`), unchanged. Windows serves a named pipe instead — `AF_UNIX` is not usable
//! from tokio, Node or CPython there, while all three speak named pipes natively:
//!
//! - **Name.** [`pipe_name_for`] maps the configured path to `\\.\pipe\lcg-<fnv1a64>`, a hash of
//!   the absolute, lower-cased, backslash-normalised path, so one workspace always gets the same
//!   pipe and two workspaces never share one. A path that already is a pipe name is used as-is.
//! - **Discovery.** On bind the service writes the pipe name to [`endpoint_file_for`] the socket
//!   path (`.lcg/service.sock` -> `.lcg/service.endpoint`), so clients in other languages read one
//!   file rather than re-implementing the hash.
//! - **Access.** Every pipe instance carries a protected DACL granting only the service's own user
//!   and SYSTEM (the default pipe DACL gives Everyone read access), refuses remote clients, and the
//!   first instance is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so another process can neither
//!   squat the name before the service binds nor share it afterwards.
//!
//! The wire protocol (newline-delimited JSON-RPC) is identical on both transports.

use std::path::{Path, PathBuf};

pub use imp::*;

/// The prefix every Windows named-pipe name carries.
#[cfg_attr(unix, allow(dead_code))]
pub const PIPE_PREFIX: &str = r"\\.\pipe\";

/// The Windows pipe name for a configured socket path (see the module docs). A client that cannot
/// read the discovery file can compute the same name: FNV-1a (64-bit) over the UTF-8 bytes of the
/// absolute path, lower-cased, with `/` replaced by `\`, formatted as 16 lower-case hex digits.
///
/// Pure and platform-independent, so the mapping is unit-tested everywhere.
#[cfg_attr(unix, allow(dead_code))]
pub fn pipe_name_for(socket_path: &str) -> String {
    if socket_path.starts_with(PIPE_PREFIX) {
        return socket_path.to_string();
    }
    let absolute = std::path::absolute(socket_path).unwrap_or_else(|_| PathBuf::from(socket_path));
    let normalised = absolute.to_string_lossy().replace('/', "\\").to_lowercase();
    format!("{PIPE_PREFIX}lcg-{:016x}", fnv1a64(normalised.as_bytes()))
}

/// Where the service records its endpoint for other-language clients: the socket path with its
/// extension replaced by `endpoint`. `None` when the configured path is already a pipe name —
/// there is nothing to discover, and `\\.\pipe\…` is a device namespace, not a directory a file
/// could be written beside.
#[cfg_attr(unix, allow(dead_code))]
pub fn endpoint_file_for(socket_path: &str) -> Option<PathBuf> {
    if socket_path.starts_with(PIPE_PREFIX) {
        return None;
    }
    Some(Path::new(socket_path).with_extension("endpoint"))
}

#[cfg_attr(unix, allow(dead_code))]
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(unix)]
mod imp {
    use std::io;

    use tokio::net::{UnixListener, UnixStream};

    pub type ServerStream = UnixStream;
    pub type ClientStream = UnixStream;

    pub struct Listener {
        inner: UnixListener,
        endpoint: String,
    }

    impl Listener {
        pub fn bind(socket_path: &str) -> io::Result<Self> {
            // A socket file left by an unclean shutdown would make bind fail with EADDRINUSE.
            let _ = std::fs::remove_file(socket_path);
            Ok(Self {
                inner: UnixListener::bind(socket_path)?,
                endpoint: socket_path.to_string(),
            })
        }

        pub fn endpoint(&self) -> &str {
            &self.endpoint
        }

        /// Cancel-safe (tokio's `UnixListener::accept` is), so it can sit in a `select!` arm.
        pub async fn accept(&mut self) -> io::Result<ServerStream> {
            self.inner.accept().await.map(|(stream, _)| stream)
        }
    }

    pub async fn connect(socket_path: &str) -> io::Result<ClientStream> {
        UnixStream::connect(socket_path).await
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::{c_void, OsStr};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::time::{Duration, Instant};

    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_PIPE_BUSY, HANDLE, HLOCAL};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub type ServerStream = NamedPipeServer;
    pub type ClientStream = NamedPipeClient;

    pub struct Listener {
        /// The instance waiting for the next client, kept present between accepts so a client
        /// arriving in that window finds a pipe instead of `ERROR_FILE_NOT_FOUND`. `None` only
        /// after creating a replacement failed; the next `accept` recreates it.
        pending: Option<NamedPipeServer>,
        endpoint: String,
    }

    impl Listener {
        pub fn bind(socket_path: &str) -> io::Result<Self> {
            let endpoint = super::pipe_name_for(socket_path);
            let pending = create_instance(&endpoint, true)?;
            if let Some(endpoint_file) = super::endpoint_file_for(socket_path) {
                std::fs::write(endpoint_file, &endpoint)?;
            }
            Ok(Self {
                pending: Some(pending),
                endpoint,
            })
        }

        pub fn endpoint(&self) -> &str {
            &self.endpoint
        }

        /// Waits for a client on the pending instance and hands it back, creating the next
        /// instance first so the pipe never has no listener.
        ///
        /// A connected client is never dropped: if creating the replacement fails, the client is
        /// still returned and `pending` is left empty, and the *next* call recreates it — so the
        /// error surfaces there, before any client is accepted, rather than stranding one here.
        /// Cancel-safe: the only await is `NamedPipeServer::connect`, and `pending` is taken only
        /// after it resolves.
        pub async fn accept(&mut self) -> io::Result<ServerStream> {
            let pending = match self.pending.as_ref() {
                Some(pending) => pending,
                None => self.pending.insert(create_instance(&self.endpoint, false)?),
            };
            pending.connect().await?;
            let connected = self
                .pending
                .take()
                .expect("pending instance was just connected");
            self.pending = match create_instance(&self.endpoint, false) {
                Ok(next) => Some(next),
                Err(e) => {
                    eprintln!(
                        "liminis-context-graph: could not create the next pipe instance for {} \
                         ({e}); retrying on the next accept",
                        self.endpoint
                    );
                    None
                }
            };
            Ok(connected)
        }
    }

    pub async fn connect(socket_path: &str) -> io::Result<ClientStream> {
        let name = client_endpoint(socket_path);
        // ERROR_PIPE_BUSY means every instance is mid-handoff; the server creates the next one
        // straight after each accept, so a short bounded retry suffices. A missing pipe still
        // fails at once (NotFound), keeping the fail-fast contract `AttachedBackend` documents.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match ClientOptions::new().open(&name) {
                Err(e)
                    if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                        && Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                other => return other,
            }
        }
    }

    /// A pipe name passes through; otherwise prefer the service's discovery file, falling back to
    /// the deterministic name when the service has not written one yet.
    fn client_endpoint(socket_path: &str) -> String {
        let Some(endpoint_file) = super::endpoint_file_for(socket_path) else {
            return socket_path.to_string();
        };
        match std::fs::read_to_string(endpoint_file) {
            Ok(recorded) if recorded.trim().starts_with(super::PIPE_PREFIX) => {
                recorded.trim().to_string()
            }
            _ => super::pipe_name_for(socket_path),
        }
    }

    fn create_instance(name: &str, first: bool) -> io::Result<NamedPipeServer> {
        let descriptor = OwnerOnlyDescriptor::new()?;
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true);
        // SAFETY: `attributes` is a valid SECURITY_ATTRIBUTES whose descriptor outlives the call.
        unsafe {
            options.create_with_security_attributes_raw(
                name,
                &mut attributes as *mut SECURITY_ATTRIBUTES as *mut c_void,
            )
        }
    }

    /// A security descriptor whose protected DACL grants GENERIC_ALL to the current process's
    /// user and to SYSTEM, and to nobody else. Allocated by the OS; freed with `LocalFree`.
    struct OwnerOnlyDescriptor(PSECURITY_DESCRIPTOR);

    impl OwnerOnlyDescriptor {
        fn new() -> io::Result<Self> {
            let sddl = to_wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{})", current_user_sid()?));
            let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: `sddl` is NUL-terminated; on success the API allocates `descriptor`.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(descriptor))
        }
    }

    impl Drop for OwnerOnlyDescriptor {
        fn drop(&mut self) {
            // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
            unsafe {
                LocalFree(self.0 as HLOCAL);
            }
        }
    }

    /// The string SID (`S-1-5-21-…`) of the user this process runs as.
    fn current_user_sid() -> io::Result<String> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: GetCurrentProcess never fails; `token` receives a handle we close below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `token` is a valid token handle opened with TOKEN_QUERY.
        let sid = unsafe { token_user_sid(token) };
        // SAFETY: `token` was opened above and is not used again.
        unsafe {
            CloseHandle(token);
        }
        sid
    }

    /// # Safety
    /// `token` must be a valid access-token handle with `TOKEN_QUERY` access.
    unsafe fn token_user_sid(token: HANDLE) -> io::Result<String> {
        let mut len = 0u32;
        // First call only reports the required buffer size (and "fails" by design).
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
        let mut buffer = vec![0u8; len as usize];
        if GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast(), len, &mut len) == 0 {
            return Err(io::Error::last_os_error());
        }
        // The byte buffer is not TOKEN_USER-aligned, so read it unaligned; the SID it points at
        // lives inside `buffer`, which outlives its use below.
        let user: TOKEN_USER = std::ptr::read_unaligned(buffer.as_ptr().cast());
        let mut wide_sid: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut wide_sid) == 0 {
            return Err(io::Error::last_os_error());
        }
        let len = (0usize..).take_while(|&i| *wide_sid.add(i) != 0).count();
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(wide_sid, len));
        LocalFree(wide_sid as HLOCAL);
        Ok(sid)
    }

    fn to_wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn fnv1a64_matches_reference_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn pipe_names_pass_through_unchanged() {
        assert_eq!(pipe_name_for(r"\\.\pipe\custom"), r"\\.\pipe\custom");
    }

    #[test]
    fn pipe_name_is_stable_across_case_and_separator_spelling() {
        let name = pipe_name_for("ws/.lcg/Service.sock");
        assert_eq!(name, pipe_name_for(r"ws\.lcg\service.sock"));
        assert!(name.starts_with(r"\\.\pipe\lcg-"), "{name}");
        assert_eq!(name.len(), PIPE_PREFIX.len() + "lcg-".len() + 16);
        assert_ne!(name, pipe_name_for("other/.lcg/service.sock"));
    }

    #[test]
    fn endpoint_file_sits_beside_the_socket_path() {
        assert_eq!(
            endpoint_file_for(".lcg/service.sock"),
            Some(PathBuf::from(".lcg/service.endpoint"))
        );
    }

    #[test]
    fn a_pipe_name_has_no_endpoint_file() {
        assert_eq!(endpoint_file_for(r"\\.\pipe\custom"), None);
    }

    #[tokio::test]
    async fn round_trips_a_line_over_the_platform_transport() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("service.sock");
        let socket_path = socket_path.to_str().unwrap().to_string();

        let mut listener = Listener::bind(&socket_path).unwrap();
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let (reader, mut writer) = tokio::io::split(stream);
            let mut line = String::new();
            BufReader::new(reader).read_line(&mut line).await.unwrap();
            writer
                .write_all(format!("echo:{line}").as_bytes())
                .await
                .unwrap();
            writer.flush().await.unwrap();
        });

        let client = connect(&socket_path).await.unwrap();
        let (reader, mut writer) = tokio::io::split(client);
        writer.write_all(b"ping\n").await.unwrap();
        writer.flush().await.unwrap();
        let mut reply = String::new();
        BufReader::new(reader).read_line(&mut reply).await.unwrap();

        assert_eq!(reply, "echo:ping\n");
        server.await.unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_second_listener_on_the_same_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("service.sock");
        let socket_path = socket_path.to_str().unwrap();

        let _first = Listener::bind(socket_path).unwrap();
        assert!(Listener::bind(socket_path).is_err());
    }

    /// `LCG_SOCKET_PATH` may itself be a pipe name (documented); binding must not try to write a
    /// discovery file into the pipe namespace, and a client given the same name must reach it.
    #[cfg(windows)]
    #[tokio::test]
    async fn binds_and_serves_a_literal_pipe_name() {
        let name = format!(r"\\.\pipe\lcg-test-literal-{}", std::process::id());

        let mut listener = Listener::bind(&name).expect("bind a literal pipe name");
        assert_eq!(listener.endpoint(), name);
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let (_reader, mut writer) = tokio::io::split(stream);
            writer.write_all(b"hello\n").await.unwrap();
            writer.flush().await.unwrap();
        });

        let client = connect(&name).await.expect("connect by pipe name");
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).await.unwrap();
        assert_eq!(line, "hello\n");
        server.await.unwrap();
    }
}
