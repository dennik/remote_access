//! The Windows service, and the helper process it starts in the console session.

use crate::config::Store;
use crate::{server, session};
use std::net::SocketAddr;
use anyhow::{anyhow, bail, Result};
use rd_common::proto::{read_msg, write_msg, ToAgent};
use std::ffi::OsString;
use std::time::Duration;
use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
use windows::core::{s, w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL, WAIT_OBJECT_0};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId, PSECURITY_DESCRIPTOR,
    SECURITY_ATTRIBUTES, TOKEN_ALL_ACCESS,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, TerminateProcess,
    WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

pub const SERVICE_NAME: &str = "RemoteAccessAgent";
const NO_SESSION: u32 = 0xFFFF_FFFF;
const STILL_ACTIVE: u32 = 259;

define_windows_service!(ffi_service_main, service_main);

pub fn run() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

fn service_main(_args: Vec<OsString>) {
    let store = Store::system();
    if let Err(e) = service_body(store.clone()) {
        store.log(&format!("service stopped with an error: {e}"));
    }
}

fn service_body(store: Store) -> Result<()> {
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let mut stop_tx = Some(stop_tx);
    let handle = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            if let Some(tx) = stop_tx.take() {
                let _ = tx.send(());
            }
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;

    let status = |state, accepted| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accepted,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(5),
        process_id: None,
    };
    handle.set_service_status(status(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN))?;

    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(async {
        tokio::select! {
            r = serve_with_helpers(store) => r,
            _ = stop_rx => Ok(()),
        }
    });
    runtime.shutdown_timeout(Duration::from_secs(2));
    handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()))?;
    result
}

/// Each session is relayed to a helper in the console session.
async fn serve_with_helpers(store: Store) -> Result<()> {
    let addr: SocketAddr = ([0, 0, 0, 0], rd_common::TCP_PORT).into();
    let discovery = (rd_common::DISCOVERY_PORT, rd_common::TCP_PORT);
    server::serve(store, addr, Some(discovery), |stream, log| relay_to_helper(stream, log)).await
}

// ------------------------------------------------------------------ helper side

/// Without this, capture and input coordinates are scaled on high-DPI displays.
pub fn set_dpi_aware() {
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

pub fn run_helper(pipe: &str) -> Result<()> {
    set_dpi_aware();
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(async {
        let client = ClientOptions::new().open(pipe)?;
        // The helper runs as SYSTEM, so it can write the service's own log.
        let store = Store::system();
        session::run(client, std::sync::Arc::new(move |line: &str| store.log(line))).await
    });
    // Capture threads may be blocked in driver calls; don't wait for them.
    runtime.shutdown_timeout(Duration::from_millis(100));
    result
}

// ------------------------------------------------------------------ service side

/// Ends the helper when the session ends, however that happens.
struct HelperProcess {
    process: HANDLE,
    session: u32,
}

impl HelperProcess {
    fn has_exited(&self) -> bool {
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(self.process, &mut code) }.map_or(true, |_| code != STILL_ACTIVE)
    }
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        unsafe {
            // The closed pipe tells the helper to release held keys and exit. Give it a moment.
            if WaitForSingleObject(self.process, 1500) != WAIT_OBJECT_0 {
                let _ = TerminateProcess(self.process, 0);
            }
            let _ = CloseHandle(self.process);
        }
    }
}

// The handle is only used through thread-safe Win32 calls.
unsafe impl Send for HelperProcess {}
unsafe impl Sync for HelperProcess {}

/// Relays a logged-in viewer to a new helper. The viewer reconnects by itself if
/// this ends, for example when the logged-in user changes.
pub async fn relay_to_helper<S>(stream: S, _store: Store) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let pipe_name = format!(r"\\.\pipe\RemoteAccess-{}", random_suffix()?);
    let pipe = create_private_pipe(&pipe_name)?;
    let helper = start_helper(&pipe_name)?;

    tokio::time::timeout(Duration::from_secs(10), pipe.connect())
        .await
        .map_err(|_| anyhow!("helper did not connect"))??;

    let (mut viewer_read, mut viewer_write) = tokio::io::split(stream);
    let (mut pipe_read, mut pipe_write) = tokio::io::split(pipe);

    let outbound = tokio::io::copy(&mut pipe_read, &mut viewer_write);
    // Parsed rather than copied because Ctrl+Alt+Del has to be sent by the service itself.
    let inbound = async {
        loop {
            let msg: ToAgent = read_msg(&mut viewer_read).await?;
            if matches!(msg, ToAgent::CtrlAltDel) {
                send_sas();
            } else {
                write_msg(&mut pipe_write, &msg).await?;
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    let watchdog = async {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if helper.has_exited() {
                return "the session helper exited";
            }
            if unsafe { WTSGetActiveConsoleSessionId() } != helper.session {
                return "the console session changed";
            }
        }
    };

    tokio::select! {
        r = outbound => { r?; Ok(()) }
        r = inbound => r,
        reason = watchdog => bail!(reason),
    }
}

fn random_suffix() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("system RNG failed: {e}"))?;
    Ok(hex::encode(bytes))
}

/// The pipe is open only to SYSTEM, which is what both ends run as.
fn create_private_pipe(name: &str) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    unsafe {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:P(A;;GA;;;SY)"),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let pipe = ServerOptions::new()
            .first_pipe_instance(true)
            .max_instances(1)
            .create_with_security_attributes_raw(name, (&mut attributes as *mut SECURITY_ATTRIBUTES).cast());
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        Ok(pipe?)
    }
}

/// Starts the helper in the console session under this service's SYSTEM token,
/// which is what lets it reach the secure desktop.
fn start_helper(pipe: &str) -> Result<HelperProcess> {
    unsafe {
        let session = WTSGetActiveConsoleSessionId();
        if session == NO_SESSION {
            bail!("no active console session");
        }

        let mut own = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own)?;
        let mut token = HANDLE::default();
        let duplicated = DuplicateTokenEx(own, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut token);
        let _ = CloseHandle(own);
        duplicated?;

        let result = (|| -> Result<HANDLE> {
            SetTokenInformation(
                token,
                TokenSessionId,
                (&session as *const u32).cast(),
                std::mem::size_of::<u32>() as u32,
            )?;

            let exe = std::env::current_exe()?;
            let mut command: Vec<u16> =
                format!("\"{}\" helper --pipe {pipe}", exe.display()).encode_utf16().chain([0]).collect();
            let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();
            let startup = STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOW>() as u32,
                lpDesktop: PWSTR(desktop.as_mut_ptr()),
                ..Default::default()
            };
            let mut info = PROCESS_INFORMATION::default();
            CreateProcessAsUserW(
                Some(token),
                PCWSTR::null(),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW,
                None,
                PCWSTR::null(),
                &startup,
                &mut info,
            )?;
            let _ = CloseHandle(info.hThread);
            Ok(info.hProcess)
        })();
        let _ = CloseHandle(token);
        Ok(HelperProcess { process: result?, session })
    }
}

/// Sends Ctrl+Alt+Del through sas.dll. `install` sets the policy that allows it.
fn send_sas() {
    unsafe {
        let Ok(library) = LoadLibraryW(w!("sas.dll")) else { return };
        if let Some(function) = GetProcAddress(library, s!("SendSAS")) {
            let send: unsafe extern "system" fn(i32) = std::mem::transmute(function);
            send(0);
        }
    }
}
