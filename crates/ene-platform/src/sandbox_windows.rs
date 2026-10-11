use super::*;
use crate::storage::os::{Local, apply_acl, wide};
use std::{
    fs,
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::ExitStatusExt,
    },
    ptr,
    time::Duration,
};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Isolation::*, *},
    Storage::FileSystem::*,
    System::{JobObjects::*, Threading::*},
};
pub type ChildStdin = NamedPipeServer;
pub type ChildStdout = NamedPipeServer;
pub struct Child {
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    process: OwnedHandle,
    job: OwnedHandle,
    profile: Profile,
    root: std::path::PathBuf,
    lease: Option<fs::File>,
}
struct Profile {
    name: Vec<u16>,
    sid: PSID,
}
impl Drop for Profile {
    fn drop(&mut self) {
        unsafe {
            DeleteAppContainerProfile(self.name.as_ptr());
            FreeSid(self.sid);
        }
    }
}
// SID allocations contain no thread-affine state.
unsafe impl Send for Profile {}
unsafe impl Sync for Profile {}
impl Child {
    pub async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        loop {
            let state = unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) };
            if state == WAIT_FAILED {
                return Err(io::Error::last_os_error());
            }
            if state == WAIT_OBJECT_0 {
                // 259 is also a valid guest exit code; it must not mean "still running".
                let mut code = 0;
                if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                return Ok(std::process::ExitStatus::from_raw(code));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    pub fn start_kill(&mut self) -> io::Result<()> {
        if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    pub async fn kill(&mut self) -> io::Result<()> {
        self.start_kill()?;
        self.wait().await.map(drop)
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        unsafe {
            TerminateJobObject(self.job.as_raw_handle(), 1);
            WaitForSingleObject(self.process.as_raw_handle(), 1000);
        }
        // Parent completion does not imply that inherited descendants released files.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION =
                unsafe { std::mem::zeroed() };
            if unsafe {
                QueryInformationJobObject(
                    self.job.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    ptr::addr_of_mut!(accounting).cast(),
                    std::mem::size_of_val(&accounting) as u32,
                    ptr::null_mut(),
                )
            } == 0
                || accounting.ActiveProcesses == 0
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.stdin.take();
        self.stdout.take();
        self.lease.take();
        let _ = fs::remove_dir_all(&self.root);
        let _ = &self.profile;
    }
}
struct Attributes {
    list: *mut std::ffi::c_void,
    _buffer: Vec<usize>,
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.list);
        }
    }
}
fn pipe(input: bool) -> io::Result<(NamedPipeServer, OwnedHandle)> {
    let name = format!(r"\\.\pipe\ene-{}", uuid::Uuid::new_v4());
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .access_inbound(!input)
        .access_outbound(input);
    let server = options.create(&name)?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let client = unsafe {
        CreateFileW(
            wide(&name).as_ptr(),
            if input { GENERIC_READ } else { GENERIC_WRITE },
            0,
            &sa,
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if client == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let client = unsafe { OwnedHandle::from_raw_handle(client) };
    use std::future::Future;
    // The synchronous client is already connected; arm Mio's server-side I/O.
    {
        let mut connection = std::pin::pin!(server.connect());
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        match connection.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(result) => result?,
            std::task::Poll::Pending => {
                return Err(io::Error::other("inherited pipe did not connect"));
            }
        }
    }
    Ok((server, client))
}
fn quote(value: &str) -> String {
    // Windows CRT argv escaping; no shell ever parses this command line.
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in value.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        if c == '"' {
            out.push_str(&"\\".repeat(slashes * 2 + 1));
        } else {
            out.push_str(&"\\".repeat(slashes));
        }
        slashes = 0;
        out.push(c);
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}
fn copy_tree(source: &Path, dest: &Path) -> io::Result<()> {
    crate::storage::check(source, true, false)?;
    crate::storage::create_private_dir_all(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if !crate::relative(&entry.file_name().to_string_lossy()) {
            return Err(io::Error::other("invalid package path"));
        }
        let target = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            let data = crate::storage::read(&entry.path(), 32 * 1024 * 1024, false)?;
            crate::storage::write_atomic(&target, &data)?;
        }
    }
    Ok(())
}

fn collect_abandoned(runtime: &Path) -> io::Result<()> {
    for item in fs::read_dir(runtime)? {
        let item = item?;
        let name = item.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_prefix("worker-")) else {
            continue;
        };
        if uuid::Uuid::parse_str(id).is_err() {
            continue;
        }
        let root = item.path();
        if crate::storage::check(&root, true, false).is_err() {
            continue;
        }
        let profile = match crate::storage::read(&root.join(".profile"), 128, true) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let Ok(profile) = String::from_utf8(profile) else {
            continue;
        };
        if !profile
            .strip_prefix("ene.worker.")
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        {
            continue;
        }
        let Ok(lock) = crate::storage::private_file(&root.join(".host-lock")) else {
            continue;
        };
        if lock.try_lock().is_err() {
            continue;
        }
        // Job handles died with the old host; this unique profile has no live lease.
        unsafe {
            DeleteAppContainerProfile(wide(&profile).as_ptr());
        }
        drop(lock);
        let _ = fs::remove_dir_all(root);
    }
    Ok(())
}
pub fn spawn(
    source: &Path,
    command: &[String],
    runner: Option<(&Path, &str)>,
) -> io::Result<Child> {
    let runtime = crate::paths::runtime()?;
    collect_abandoned(&runtime)?;
    if runner.is_none() && !super::command_supported(command) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows plugins require a packaged Windows executable",
        ));
    }
    let id = format!("ene.worker.{}", uuid::Uuid::new_v4());
    let name = wide(&id);
    let mut sid = ptr::null_mut();
    let hr = unsafe {
        CreateAppContainerProfile(
            name.as_ptr(),
            name.as_ptr(),
            name.as_ptr(),
            ptr::null(),
            0,
            &mut sid,
        )
    };
    if hr < 0 {
        return Err(io::Error::other(format!(
            "CreateAppContainerProfile: {hr:#x}"
        )));
    }
    // CreateAppContainerProfile returns a SID freed with FreeSid (not LocalFree).
    let profile = Profile { name, sid };
    let mut sid_text = ptr::null_mut();
    if unsafe {
        windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW(sid, &mut sid_text)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let sid_allocation = Local(sid_text.cast());
    let mut len = 0;
    unsafe {
        while *sid_text.add(len) != 0 {
            len += 1;
        }
    }
    let sid_string = String::from_utf16(unsafe { std::slice::from_raw_parts(sid_text, len) })
        .map_err(io::Error::other)?;
    drop(sid_allocation);
    let root = runtime.join(format!("worker-{}", uuid::Uuid::new_v4()));
    crate::storage::create_private_dir_all(&root)?;
    // A guard covers every failed setup operation, before there is a process.
    struct Cleanup {
        root: Option<std::path::PathBuf>,
        lease: Option<fs::File>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.lease.take();
            if let Some(root) = &self.root {
                let _ = fs::remove_dir_all(root);
            }
        }
    }
    let mut cleanup = Cleanup {
        root: Some(root.clone()),
        lease: None,
    };
    let lease = crate::storage::private_file(&root.join(".host-lock"))?;
    lease.try_lock().map_err(io::Error::other)?;
    cleanup.lease = Some(lease);
    crate::storage::write_atomic(&root.join(".profile"), id.as_bytes())?;
    let package = root.join("plugin");
    copy_tree(source, &package)?;
    let temp = root.join("tmp");
    crate::storage::create_private_dir_all(&temp)?;
    // Windows redirects TEMP under LOCALAPPDATA/Packages/<profile>/AC/Temp.
    // Prepare that location inside this worker's disposable data root.
    crate::storage::create_private_dir_all(&temp.join("Packages").join(&id).join("AC/Temp"))?;
    let (binary, args) = if let Some((runner, entrypoint)) = runner {
        let binary = root.join("ene-plugin-runner.exe");
        fs::copy(runner, &binary)?;
        (
            binary,
            vec![
                "--package".into(),
                package.to_string_lossy().into_owned(),
                "--entrypoint".into(),
                entrypoint.into(),
            ],
        )
    } else {
        let translated = command
            .iter()
            .map(|arg| {
                if let Some(path) = arg.strip_prefix("/plugin/") {
                    if !crate::relative(path) {
                        return Err(io::Error::other("invalid logical package path"));
                    }
                    Ok(package.join(path).to_string_lossy().into_owned())
                } else {
                    Ok(arg.clone())
                }
            })
            .collect::<io::Result<Vec<_>>>()?;
        (
            std::path::PathBuf::from(&translated[0]),
            translated[1..].to_vec(),
        )
    };
    if !binary
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
        || !binary.is_file()
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows plugin executable must be .exe",
        ));
    }
    fn grant(path: &Path, sid: &str) -> io::Result<()> {
        apply_acl(path, Some((sid, false)), true)?;
        if path.is_dir() {
            for item in fs::read_dir(path)? {
                grant(&item?.path(), sid)?;
            }
        }
        Ok(())
    }
    // Never grant ALL APPLICATION PACKAGES access to ene data.
    apply_acl(&root, Some((&sid_string, false)), true)?;
    for item in fs::read_dir(&root)? {
        let item = item?;
        if ![".host-lock", ".profile"]
            .iter()
            .any(|name| item.file_name() == *name)
        {
            grant(&item.path(), &sid_string)?;
        }
    }
    fn writable(path: &Path, sid: &str) -> io::Result<()> {
        apply_acl(path, Some((sid, true)), false)?;
        for item in fs::read_dir(path)? {
            writable(&item?.path(), sid)?;
        }
        Ok(())
    }
    writable(&temp, &sid_string)?;
    let (stdin, input_handle) = pipe(true)?;
    let (stdout, output_handle) = pipe(false)?;
    let null = unsafe {
        CreateFileW(
            wide("NUL").as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: ptr::null_mut(),
                bInheritHandle: 1,
            },
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if null == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let null = unsafe { OwnedHandle::from_raw_handle(null) };
    let handles = [
        input_handle.as_raw_handle(),
        output_handle.as_raw_handle(),
        null.as_raw_handle(),
    ];
    let capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: profile.sid,
        Capabilities: ptr::null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };
    let mut size = 0;
    unsafe {
        InitializeProcThreadAttributeList(ptr::null_mut(), 2, 0, &mut size);
    }
    let mut buffer = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
    let list = buffer.as_mut_ptr().cast();
    if unsafe { InitializeProcThreadAttributeList(list, 2, 0, &mut size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let attributes = Attributes {
        list,
        _buffer: buffer,
    };
    unsafe {
        if UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            ptr::addr_of!(capabilities).cast_mut().cast(),
            std::mem::size_of_val(&capabilities),
            ptr::null_mut(),
            ptr::null(),
        ) == 0
            || UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast_mut().cast(),
                std::mem::size_of_val(&handles),
                ptr::null_mut(),
                ptr::null(),
            ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = handles[0];
    startup.StartupInfo.hStdOutput = handles[1];
    startup.StartupInfo.hStdError = handles[2];
    startup.lpAttributeList = list;
    let line = std::iter::once(binary.to_string_lossy().into_owned())
        .chain(args)
        .map(|a| quote(&a))
        .collect::<Vec<_>>()
        .join(" ");
    let mut line = wide(line);
    let system_root = std::env::var("SystemRoot").map_err(io::Error::other)?;
    let environment = [
        format!("HOME={}", temp.display()),
        format!("LOCALAPPDATA={}", temp.display()),
        format!("SystemRoot={system_root}"),
        format!("TEMP={}", temp.display()),
        format!("TMP={}", temp.display()),
    ]
    .join("\0")
        + "\0\0";
    let environment: Vec<u16> = environment.encode_utf16().collect();
    let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            ptr::addr_of!(limits).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        CreateProcessW(
            wide(&binary).as_ptr(),
            line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            CREATE_SUSPENDED
                | CREATE_NO_WINDOW
                | CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            wide(&package).as_ptr(),
            &startup.StartupInfo,
            &mut pi,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess) };
    let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread) };
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) } == 0 {
        let error = io::Error::last_os_error();
        unsafe {
            TerminateProcess(process.as_raw_handle(), 1);
        }
        return Err(error);
    }
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    drop(attributes);
    drop(input_handle);
    drop(output_handle);
    drop(null);
    cleanup.root = None;
    let lease = cleanup.lease.take();
    Ok(Child {
        stdin: Some(stdin),
        stdout: Some(stdout),
        process,
        job,
        profile,
        root,
        lease,
    })
}
