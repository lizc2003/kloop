use std::cmp::Ordering as CmpOrdering;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::os::windows::io::FromRawHandle as _;
#[cfg(test)]
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::DuplicateHandle;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::SetHandleInformation;
use windows_sys::Win32::Foundation::DBG_CONTINUE;
use windows_sys::Win32::Foundation::DBG_EXCEPTION_NOT_HANDLED;
use windows_sys::Win32::Foundation::DUPLICATE_SAME_ACCESS;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
use windows_sys::Win32::Foundation::WAIT_FAILED;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::Globalization::CompareStringOrdinal;
use windows_sys::Win32::Globalization::CSTR_EQUAL;
use windows_sys::Win32::Globalization::CSTR_GREATER_THAN;
use windows_sys::Win32::Globalization::CSTR_LESS_THAN;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
use windows_sys::Win32::System::Diagnostics::Debug::ContinueDebugEvent;
use windows_sys::Win32::System::Diagnostics::Debug::DebugSetProcessKillOnExit;
use windows_sys::Win32::System::Diagnostics::Debug::WaitForDebugEvent;
use windows_sys::Win32::System::Diagnostics::Debug::CREATE_PROCESS_DEBUG_EVENT;
use windows_sys::Win32::System::Diagnostics::Debug::DEBUG_EVENT;
use windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_DEBUG_EVENT;
use windows_sys::Win32::System::Diagnostics::Debug::EXIT_PROCESS_DEBUG_EVENT;
use windows_sys::Win32::System::Diagnostics::Debug::LOAD_DLL_DEBUG_EVENT;
use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
use windows_sys::Win32::System::JobObjects::CreateJobObjectW;
use windows_sys::Win32::System::JobObjects::IsProcessInJob;
use windows_sys::Win32::System::JobObjects::JobObjectBasicAccountingInformation;
use windows_sys::Win32::System::JobObjects::JobObjectExtendedLimitInformation;
use windows_sys::Win32::System::JobObjects::QueryInformationJobObject;
use windows_sys::Win32::System::JobObjects::SetInformationJobObject;
use windows_sys::Win32::System::JobObjects::TerminateJobObject;
use windows_sys::Win32::System::JobObjects::JOBOBJECT_BASIC_ACCOUNTING_INFORMATION;
use windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
#[cfg(test)]
use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
#[cfg(test)]
use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::CreateProcessW;
use windows_sys::Win32::System::Threading::DeleteProcThreadAttributeList;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::GetExitCodeProcess;
use windows_sys::Win32::System::Threading::InitializeProcThreadAttributeList;
use windows_sys::Win32::System::Threading::OpenProcess;
use windows_sys::Win32::System::Threading::ResumeThread;
use windows_sys::Win32::System::Threading::TerminateProcess;
use windows_sys::Win32::System::Threading::UpdateProcThreadAttribute;
use windows_sys::Win32::System::Threading::WaitForSingleObject;
use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
use windows_sys::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT;
use windows_sys::Win32::System::Threading::DEBUG_PROCESS;
use windows_sys::Win32::System::Threading::EXTENDED_STARTUPINFO_PRESENT;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
use windows_sys::Win32::System::Threading::PROCESS_SET_QUOTA;
use windows_sys::Win32::System::Threading::PROCESS_TERMINATE;
use windows_sys::Win32::System::Threading::PROC_THREAD_ATTRIBUTE_HANDLE_LIST;
use windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
use windows_sys::Win32::System::Threading::STARTUPINFOEXW;
#[cfg(test)]
use windows_sys::Win32::System::Threading::STARTUPINFOW;

use super::ProcessExit;
use super::ProcessPipe;
use super::ProcessSpec;
use super::ProcessStdio;

#[derive(Debug)]
struct OwnedHandle(HANDLE);

unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    fn new(handle: HANDLE) -> io::Result<Self> {
        if handle == 0 || handle == -1 {
            Err(last_error())
        } else {
            Ok(Self(handle))
        }
    }

    fn raw(&self) -> HANDLE {
        self.0
    }

    fn duplicate(&self, inheritable: bool) -> io::Result<Self> {
        duplicate_handle(self.0, inheritable)
    }

    fn into_file(self) -> File {
        let handle = self.0;
        std::mem::forget(self);
        unsafe { File::from_raw_handle(handle as _) }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct Job {
    handle: OwnedHandle,
}

struct JobSet {
    root: Job,
    descendants: Job,
    terminated: AtomicBool,
}

unsafe impl Send for Job {}
unsafe impl Sync for Job {}
unsafe impl Send for JobSet {}
unsafe impl Sync for JobSet {}

#[derive(Clone)]
pub(super) struct Killer {
    jobs: Arc<JobSet>,
}

pub(super) struct Child {
    process: OwnedHandle,
    killer: Killer,
    debugger: Option<Debugger>,
    stdout: Option<ProcessPipe>,
    stderr: Option<ProcessPipe>,
    waiter: Option<tokio::task::JoinHandle<io::Result<()>>>,
    exit: Option<ProcessExit>,
}

struct ChildStdio {
    stdin: OwnedHandle,
    stdout: OwnedHandle,
    stderr: OwnedHandle,
    parent_stdout: Option<File>,
    parent_stderr: Option<File>,
}

struct Debugger {
    thread: Option<JoinHandle<()>>,
    failure: Arc<Mutex<Option<DebugFailure>>>,
}

#[derive(Clone)]
struct DebugFailure {
    raw_os_error: Option<i32>,
    message: String,
}

struct DebugSpawned {
    process: OwnedHandle,
    jobs: Arc<JobSet>,
    stdout: Option<File>,
    stderr: Option<File>,
}

struct DebugThreadRoot {
    process: OwnedHandle,
    jobs: Arc<JobSet>,
    root_pid: u32,
    stdout: Option<File>,
    stderr: Option<File>,
}

struct AttributeList {
    storage: Vec<usize>,
    handles: Box<[HANDLE]>,
    ptr: windows_sys::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl AttributeList {
    fn for_handles(handles: &[HANDLE]) -> io::Result<Self> {
        let handles = handles.to_vec().into_boxed_slice();
        let mut bytes = 0usize;
        unsafe {
            InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut bytes);
        }
        if bytes == 0 {
            return Err(last_error());
        }
        let words = bytes.div_ceil(size_of::<usize>());
        let mut storage = vec![0usize; words];
        let list = storage.as_mut_ptr() as _;
        if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut bytes) } == 0 {
            return Err(last_error());
        }
        let updated = unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                std::mem::size_of_val(handles.as_ref()),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if updated == 0 {
            let error = last_error();
            unsafe {
                DeleteProcThreadAttributeList(list);
            }
            return Err(error);
        }
        Ok(Self {
            storage,
            handles,
            ptr: list,
        })
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.ptr);
        }
        let _ = (&self.storage, &self.handles);
    }
}

pub(super) fn spawn(spec: ProcessSpec) -> io::Result<Child> {
    spawn_impl(spec, SpawnFault::None)
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SpawnFault {
    None,
    BeforeAssign,
    BeforeResume,
    RealAssignFailure,
    DebugDescendantBreakaway,
}

#[cfg(not(test))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum SpawnFault {
    None,
}

#[cfg(test)]
pub(super) fn spawn_with_fault(spec: ProcessSpec, fault: SpawnFault) -> io::Result<Child> {
    spawn_impl(spec, fault)
}

fn spawn_impl(spec: ProcessSpec, fault: SpawnFault) -> io::Result<Child> {
    #[cfg(not(test))]
    let _ = fault;
    if spec.windows_debug_descendants {
        return spawn_debugged(spec, fault);
    }
    let ProcessSpec {
        executable,
        args,
        cwd,
        env_add,
        env_remove,
        stdin,
        stdout,
        stderr,
        windows_debug_descendants: _,
    } = spec;
    if !executable.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows process executable must be absolute",
        ));
    }
    if !matches!(stdin, ProcessStdio::Null) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows process stdin currently must be null",
        ));
    }

    let jobs = Arc::new(create_job_set()?);
    #[cfg(test)]
    let _full_job_member = if fault == SpawnFault::RealAssignFailure {
        Some(
            fill_job_to_active_process_limit(&jobs.root, &cwd).map_err(|error| {
                io::Error::other(format!(
                    "cannot prepare real Job assignment failure: {error}"
                ))
            })?,
        )
    } else {
        None
    };
    let creation_guard = kloop_process_spawn::lock();
    let stdio = prepare_stdio(stdout, stderr)?;
    let attributes =
        AttributeList::for_handles(&[stdio.stdin.raw(), stdio.stdout.raw(), stdio.stderr.raw()])?;
    let executable_wide = wide_nul(executable.as_os_str(), "executable")?;
    let cwd_wide = wide_nul(cwd.as_os_str(), "working directory")?;
    let mut command_line = command_line(executable.as_os_str(), &args)?;
    let environment = environment_block(env_add, env_remove)?;

    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdio.stdin.raw();
    startup.StartupInfo.hStdOutput = stdio.stdout.raw();
    startup.StartupInfo.hStdError = stdio.stderr.raw();
    startup.lpAttributeList = attributes.ptr;
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let created = unsafe {
        CreateProcessW(
            executable_wide.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            cwd_wide.as_ptr(),
            (&startup as *const STARTUPINFOEXW).cast(),
            &mut info,
        )
    };
    if created == 0 {
        return Err(last_error());
    }
    let process = OwnedHandle::new(info.hProcess)?;
    let thread = OwnedHandle::new(info.hThread)?;

    #[cfg(test)]
    if fault == SpawnFault::BeforeAssign {
        unsafe {
            TerminateProcess(process.raw(), 1);
            WaitForSingleObject(process.raw(), INFINITE);
        }
        return Err(io::Error::other("injected failure before Job assignment"));
    }

    if unsafe { AssignProcessToJobObject(jobs.root.handle.raw(), process.raw()) } == 0 {
        let error = last_error();
        unsafe {
            TerminateProcess(process.raw(), 1);
            WaitForSingleObject(process.raw(), INFINITE);
        }
        return Err(error);
    }
    #[cfg(test)]
    if fault == SpawnFault::RealAssignFailure {
        unsafe {
            TerminateJobObject(jobs.root.handle.raw(), 1);
            WaitForSingleObject(process.raw(), INFINITE);
        }
        return Err(io::Error::other(
            "expected active-process limit to reject Job assignment",
        ));
    }
    #[cfg(test)]
    if fault == SpawnFault::BeforeResume {
        unsafe {
            TerminateJobObject(jobs.root.handle.raw(), 1);
            WaitForSingleObject(process.raw(), INFINITE);
        }
        return Err(io::Error::other("injected failure before root resume"));
    }
    if unsafe { ResumeThread(thread.raw()) } == u32::MAX {
        let error = last_error();
        terminate_job_and_wait(&jobs.root, &process);
        return Err(error);
    }
    drop(thread);
    drop(attributes);
    drop(stdio.stdin);
    drop(stdio.stdout);
    drop(stdio.stderr);
    drop(creation_guard);

    let stdout = stdio
        .parent_stdout
        .map(tokio::fs::File::from_std)
        .map(|pipe| Box::pin(pipe) as ProcessPipe);
    let stderr = stdio
        .parent_stderr
        .map(tokio::fs::File::from_std)
        .map(|pipe| Box::pin(pipe) as ProcessPipe);
    Ok(Child {
        process,
        killer: Killer { jobs },
        debugger: None,
        stdout,
        stderr,
        waiter: None,
        exit: None,
    })
}

fn spawn_debugged(spec: ProcessSpec, fault: SpawnFault) -> io::Result<Child> {
    let failure = Arc::new(Mutex::new(None));
    let thread_failure = failure.clone();
    let (startup_tx, startup_rx) = mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("kloop-job-debugger".into())
        .spawn(move || match create_debugged_root(spec, fault) {
            Ok(root) => {
                let parent_process = match root.process.duplicate(false) {
                    Ok(process) => process,
                    Err(error) => {
                        let _ = startup_tx.send(Err(error));
                        return;
                    }
                };
                let spawned = DebugSpawned {
                    process: parent_process,
                    jobs: root.jobs.clone(),
                    stdout: root.stdout,
                    stderr: root.stderr,
                };
                if startup_tx.send(Ok(spawned)).is_err() {
                    terminate_jobs(&root.jobs);
                    return;
                }
                if let Err(error) = debug_descendants(root.root_pid, &root.jobs) {
                    record_debug_failure(&thread_failure, &error);
                    terminate_jobs(&root.jobs);
                }
            }
            Err(error) => {
                let _ = startup_tx.send(Err(error));
            }
        })
        .map_err(|error| {
            io::Error::other(format!("cannot start Windows descendant debugger: {error}"))
        })?;

    let spawned = match startup_rx.recv() {
        Ok(Ok(spawned)) => spawned,
        Ok(Err(error)) => {
            let _ = thread.join();
            return Err(error);
        }
        Err(error) => {
            let _ = thread.join();
            return Err(io::Error::other(format!(
                "Windows descendant debugger startup channel closed: {error}"
            )));
        }
    };
    let stdout = spawned
        .stdout
        .map(tokio::fs::File::from_std)
        .map(|pipe| Box::pin(pipe) as ProcessPipe);
    let stderr = spawned
        .stderr
        .map(tokio::fs::File::from_std)
        .map(|pipe| Box::pin(pipe) as ProcessPipe);
    Ok(Child {
        process: spawned.process,
        killer: Killer { jobs: spawned.jobs },
        debugger: Some(Debugger {
            thread: Some(thread),
            failure,
        }),
        stdout,
        stderr,
        waiter: None,
        exit: None,
    })
}

fn create_debugged_root(spec: ProcessSpec, fault: SpawnFault) -> io::Result<DebugThreadRoot> {
    #[cfg(not(test))]
    let _ = fault;
    let ProcessSpec {
        executable,
        args,
        cwd,
        env_add,
        env_remove,
        stdin,
        stdout,
        stderr,
        windows_debug_descendants: _,
    } = spec;
    if !executable.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows process executable must be absolute",
        ));
    }
    if !matches!(stdin, ProcessStdio::Null) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows process stdin currently must be null",
        ));
    }
    let jobs = Arc::new(create_job_set()?);
    #[cfg(test)]
    if fault == SpawnFault::DebugDescendantBreakaway {
        enable_silent_breakaway(&jobs.root)?;
    }
    #[cfg(test)]
    let _full_job_member = if fault == SpawnFault::RealAssignFailure {
        Some(
            fill_job_to_active_process_limit(&jobs.root, &cwd).map_err(|error| {
                io::Error::other(format!(
                    "cannot prepare real Job assignment failure: {error}"
                ))
            })?,
        )
    } else {
        None
    };
    let creation_guard = kloop_process_spawn::lock();
    let mut stdio = prepare_stdio(stdout, stderr)?;
    let attributes =
        AttributeList::for_handles(&[stdio.stdin.raw(), stdio.stdout.raw(), stdio.stderr.raw()])?;
    let executable_wide = wide_nul(executable.as_os_str(), "executable")?;
    let cwd_wide = wide_nul(cwd.as_os_str(), "working directory")?;
    let mut command_line = command_line(executable.as_os_str(), &args)?;
    let environment = environment_block(env_add, env_remove)?;

    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdio.stdin.raw();
    startup.StartupInfo.hStdOutput = stdio.stdout.raw();
    startup.StartupInfo.hStdError = stdio.stderr.raw();
    startup.lpAttributeList = attributes.ptr;
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        CreateProcessW(
            executable_wide.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            DEBUG_PROCESS
                | CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            cwd_wide.as_ptr(),
            (&startup as *const STARTUPINFOEXW).cast(),
            &mut info,
        )
    } == 0
    {
        return Err(last_error());
    }
    let process = OwnedHandle::new(info.hProcess)?;
    let thread = OwnedHandle::new(info.hThread)?;
    if unsafe { DebugSetProcessKillOnExit(1) } == 0 {
        let error = io::Error::other(format!(
            "cannot make Windows descendant debugger fail closed: {}",
            last_error()
        ));
        unsafe {
            TerminateProcess(process.raw(), 1);
        }
        return Err(error);
    }

    #[cfg(test)]
    if fault == SpawnFault::BeforeAssign {
        unsafe {
            TerminateProcess(process.raw(), 1);
        }
        return Err(io::Error::other("injected failure before Job assignment"));
    }
    if unsafe { AssignProcessToJobObject(jobs.root.handle.raw(), process.raw()) } == 0 {
        let error = last_error();
        unsafe {
            TerminateProcess(process.raw(), 1);
        }
        return Err(error);
    }
    #[cfg(test)]
    if fault == SpawnFault::RealAssignFailure {
        unsafe {
            TerminateJobObject(jobs.root.handle.raw(), 1);
        }
        return Err(io::Error::other(
            "expected active-process limit to reject Job assignment",
        ));
    }
    #[cfg(test)]
    if fault == SpawnFault::BeforeResume {
        unsafe {
            TerminateJobObject(jobs.root.handle.raw(), 1);
        }
        return Err(io::Error::other("injected failure before root resume"));
    }
    if unsafe { ResumeThread(thread.raw()) } == u32::MAX {
        let error = last_error();
        unsafe {
            TerminateJobObject(jobs.root.handle.raw(), 1);
        }
        return Err(error);
    }
    drop(thread);
    drop(attributes);
    drop(stdio.stdin);
    drop(stdio.stdout);
    drop(stdio.stderr);
    drop(creation_guard);

    Ok(DebugThreadRoot {
        process,
        jobs,
        root_pid: info.dwProcessId,
        stdout: stdio.parent_stdout.take(),
        stderr: stdio.parent_stderr.take(),
    })
}

impl Debugger {
    fn finish(&mut self) -> io::Result<()> {
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| io::Error::other("Windows descendant debugger panicked"))?;
        }
        let failure = self
            .failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        match failure {
            Some(failure) => Err(failure.into_error()),
            None => Ok(()),
        }
    }
}

impl DebugFailure {
    fn from_error(error: &io::Error) -> Self {
        Self {
            raw_os_error: error.raw_os_error(),
            message: error.to_string(),
        }
    }

    fn into_error(self) -> io::Error {
        match self.raw_os_error {
            Some(code) => io::Error::other(format!(
                "{} ({})",
                self.message,
                io::Error::from_raw_os_error(code)
            )),
            None => io::Error::other(self.message),
        }
    }
}

fn record_debug_failure(failure: &Mutex<Option<DebugFailure>>, error: &io::Error) {
    let mut failure = failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if failure.is_none() {
        *failure = Some(DebugFailure::from_error(error));
    }
}

fn debug_descendants(root_pid: u32, jobs: &JobSet) -> io::Result<()> {
    let mut processes = HashSet::new();
    loop {
        let mut event: DEBUG_EVENT = unsafe { std::mem::zeroed() };
        if unsafe { WaitForDebugEvent(&mut event, INFINITE) } == 0 {
            return Err(io::Error::other(format!(
                "Windows descendant debugger wait failed: {}",
                last_error()
            )));
        }

        let mut event_failure = None;
        let mut all_exited = false;
        match event.dwDebugEventCode {
            CREATE_PROCESS_DEBUG_EVENT => {
                let info = unsafe { event.u.CreateProcessInfo };
                close_debug_file(info.hFile);
                processes.insert(event.dwProcessId);
                if event.dwProcessId != root_pid {
                    match OwnedHandle::new(unsafe {
                        OpenProcess(
                            PROCESS_QUERY_LIMITED_INFORMATION
                                | PROCESS_SET_QUOTA
                                | PROCESS_TERMINATE,
                            0,
                            event.dwProcessId,
                        )
                    }) {
                        Err(error) => {
                            event_failure = Some(io::Error::other(format!(
                                "cannot open suspended Windows descendant for Job assignment: {error}"
                            )));
                        }
                        Ok(process) => {
                            let in_job =
                                process_in_job(process.raw(), &jobs.root).and_then(|in_root| {
                                    if in_root {
                                        Ok(true)
                                    } else {
                                        process_in_job(process.raw(), &jobs.descendants)
                                    }
                                });
                            if let Err(error) = in_job {
                                event_failure = Some(error);
                            } else if !in_job.expect("Job membership was checked")
                                && unsafe {
                                    AssignProcessToJobObject(
                                        jobs.descendants.handle.raw(),
                                        process.raw(),
                                    )
                                } == 0
                            {
                                event_failure = Some(io::Error::other(format!(
                                    "cannot assign suspended Windows descendant to containment Job: {}",
                                    last_error()
                                )));
                            }
                        }
                    }
                    if event_failure.is_some() {
                        unsafe {
                            TerminateProcess(info.hProcess, 1);
                        }
                        terminate_jobs(jobs);
                    }
                }
            }
            LOAD_DLL_DEBUG_EVENT => {
                close_debug_file(unsafe { event.u.LoadDll.hFile });
            }
            EXIT_PROCESS_DEBUG_EVENT => {
                processes.remove(&event.dwProcessId);
                all_exited = processes.is_empty();
            }
            _ => {}
        }

        let status = if event.dwDebugEventCode == EXCEPTION_DEBUG_EVENT {
            DBG_EXCEPTION_NOT_HANDLED
        } else {
            DBG_CONTINUE
        };
        if unsafe { ContinueDebugEvent(event.dwProcessId, event.dwThreadId, status) } == 0 {
            return Err(io::Error::other(format!(
                "Windows descendant debugger continue failed: {}",
                last_error()
            )));
        }
        if let Some(error) = event_failure {
            return Err(error);
        }
        if all_exited {
            return Ok(());
        }
    }
}

fn process_in_job(process: HANDLE, job: &Job) -> io::Result<bool> {
    let mut in_job = 0;
    if unsafe { IsProcessInJob(process, job.handle.raw(), &mut in_job) } == 0 {
        return Err(io::Error::other(format!(
            "cannot inspect Windows descendant Job membership: {}",
            last_error()
        )));
    }
    Ok(in_job != 0)
}

fn close_debug_file(handle: HANDLE) {
    if handle != 0 && handle != -1 {
        unsafe {
            CloseHandle(handle);
        }
    }
}

fn terminate_job_and_wait(job: &Job, process: &OwnedHandle) {
    unsafe {
        TerminateJobObject(job.handle.raw(), 1);
        WaitForSingleObject(process.raw(), INFINITE);
    }
}

fn terminate_jobs(jobs: &JobSet) {
    unsafe {
        TerminateJobObject(jobs.descendants.handle.raw(), 1);
        TerminateJobObject(jobs.root.handle.raw(), 1);
    }
}

fn create_job_set() -> io::Result<JobSet> {
    Ok(JobSet {
        root: create_job()?,
        descendants: create_job()?,
        terminated: AtomicBool::new(false),
    })
}

fn create_job() -> io::Result<Job> {
    let handle = OwnedHandle::new(unsafe { CreateJobObjectW(ptr::null(), ptr::null()) })?;
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            handle.raw(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(last_error());
    }
    Ok(Job { handle })
}

#[cfg(test)]
fn enable_silent_breakaway(job: &Job) -> io::Result<()> {
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
    if unsafe {
        SetInformationJobObject(
            job.handle.raw(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(last_error());
    }
    Ok(())
}

#[cfg(test)]
fn fill_job_to_active_process_limit(
    job: &Job,
    cwd: &std::path::Path,
) -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    if unsafe {
        SetInformationJobObject(
            job.handle.raw(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(last_error());
    }

    let executable =
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
            .join("System32")
            .join("cmd.exe");
    let executable_wide = wide_nul(executable.as_os_str(), "test executable")?;
    let cwd_wide = wide_nul(cwd.as_os_str(), "test working directory")?;
    let mut command_line = command_line(
        executable.as_os_str(),
        &["/D".into(), "/C".into(), "exit 0".into()],
    )?;
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        CreateProcessW(
            executable_wide.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            CREATE_SUSPENDED,
            ptr::null(),
            cwd_wide.as_ptr(),
            &startup,
            &mut info,
        )
    } == 0
    {
        return Err(last_error());
    }
    let process = OwnedHandle::new(info.hProcess)?;
    let thread = OwnedHandle::new(info.hThread)?;
    if unsafe { AssignProcessToJobObject(job.handle.raw(), process.raw()) } == 0 {
        let error = last_error();
        unsafe {
            TerminateProcess(process.raw(), 1);
            WaitForSingleObject(process.raw(), INFINITE);
        }
        return Err(error);
    }
    Ok((process, thread))
}

fn prepare_stdio(stdout: ProcessStdio, stderr: ProcessStdio) -> io::Result<ChildStdio> {
    let stdin = open_null(GENERIC_READ)?;
    let (stdout, parent_stdout) = prepare_output(stdout)?;
    let (stderr, parent_stderr) = prepare_output(stderr)?;
    Ok(ChildStdio {
        stdin,
        stdout,
        stderr,
        parent_stdout,
        parent_stderr,
    })
}

fn prepare_output(stdio: ProcessStdio) -> io::Result<(OwnedHandle, Option<File>)> {
    match stdio {
        ProcessStdio::Null => Ok((open_null(GENERIC_WRITE)?, None)),
        ProcessStdio::Pipe => {
            let security = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: ptr::null_mut(),
                bInheritHandle: 1,
            };
            let mut read = 0;
            let mut write = 0;
            if unsafe { CreatePipe(&mut read, &mut write, &security, 0) } == 0 {
                return Err(last_error());
            }
            let read = OwnedHandle::new(read)?;
            let write = OwnedHandle::new(write)?;
            if unsafe { SetHandleInformation(read.raw(), HANDLE_FLAG_INHERIT, 0) } == 0 {
                return Err(last_error());
            }
            Ok((write, Some(read.into_file())))
        }
        ProcessStdio::File(file) => {
            let source = file.as_raw_handle() as HANDLE;
            let inherited = duplicate_handle(source, true)?;
            Ok((inherited, None))
        }
    }
}

fn open_null(access: u32) -> io::Result<OwnedHandle> {
    let name: Vec<u16> = OsStr::new("NUL").encode_wide().chain(Some(0)).collect();
    let security = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    OwnedHandle::new(unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &security,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            0,
        )
    })
}

fn duplicate_handle(source: HANDLE, inheritable: bool) -> io::Result<OwnedHandle> {
    let process = unsafe { GetCurrentProcess() };
    let mut duplicate = 0;
    if unsafe {
        DuplicateHandle(
            process,
            source,
            process,
            &mut duplicate,
            0,
            i32::from(inheritable),
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(last_error());
    }
    OwnedHandle::new(duplicate)
}

fn wide_nul(value: &OsStr, label: &str) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = value.encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} contains NUL"),
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn command_line(executable: &OsStr, args: &[OsString]) -> io::Result<Vec<u16>> {
    let mut line = Vec::new();
    append_quoted_arg(&mut line, executable, "executable")?;
    for arg in args {
        line.push(b' ' as u16);
        append_quoted_arg(&mut line, arg, "argument")?;
    }
    line.push(0);
    Ok(line)
}

fn append_quoted_arg(out: &mut Vec<u16>, arg: &OsStr, label: &str) -> io::Result<()> {
    let wide: Vec<u16> = arg.encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} contains NUL"),
        ));
    }
    let quote = wide.is_empty()
        || wide
            .iter()
            .any(|unit| matches!(*unit, 0x20 | 0x09 | 0x0a | 0x0b | 0x0c | 0x0d | 0x22));
    if !quote {
        out.extend(wide);
        return Ok(());
    }
    out.push(b'"' as u16);
    let mut backslashes = 0usize;
    for unit in wide {
        if unit == b'\\' as u16 {
            backslashes += 1;
            continue;
        }
        if unit == b'"' as u16 {
            out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            out.push(unit);
        } else {
            out.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
            out.push(unit);
        }
        backslashes = 0;
    }
    out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    out.push(b'"' as u16);
    Ok(())
}

#[derive(Clone, Debug, Eq)]
struct EnvKey {
    utf16: Vec<u16>,
}

impl From<OsString> for EnvKey {
    fn from(value: OsString) -> Self {
        Self {
            utf16: value.encode_wide().collect(),
        }
    }
}

impl From<&OsStr> for EnvKey {
    fn from(value: &OsStr) -> Self {
        Self::from(value.to_os_string())
    }
}

impl Ord for EnvKey {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        let result = unsafe {
            CompareStringOrdinal(
                self.utf16.as_ptr(),
                self.utf16.len() as i32,
                other.utf16.as_ptr(),
                other.utf16.len() as i32,
                1,
            )
        };
        match result {
            CSTR_LESS_THAN => CmpOrdering::Less,
            CSTR_EQUAL => CmpOrdering::Equal,
            CSTR_GREATER_THAN => CmpOrdering::Greater,
            _ => panic!(
                "comparing Windows environment keys failed: {}",
                last_error()
            ),
        }
    }
}

impl PartialOrd for EnvKey {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for EnvKey {
    fn eq(&self, other: &Self) -> bool {
        self.utf16.len() == other.utf16.len() && self.cmp(other) == CmpOrdering::Equal
    }
}

fn environment_block(
    env_add: Vec<(OsString, OsString)>,
    env_remove: Vec<OsString>,
) -> io::Result<Vec<u16>> {
    let mut environment: BTreeMap<EnvKey, OsString> = BTreeMap::new();
    for (name, value) in std::env::vars_os() {
        environment.insert(EnvKey::from(name), value);
    }
    for (name, value) in env_add {
        environment.insert(EnvKey::from(name), value);
    }
    for name in env_remove {
        environment.remove(&EnvKey::from(name));
    }
    let mut block = Vec::new();
    for (name, value) in environment {
        let name = name.utf16;
        let value: Vec<u16> = value.encode_wide().collect();
        let invalid_equals = name
            .iter()
            .enumerate()
            .any(|(index, unit)| *unit == b'=' as u16 && index != 0);
        if name.contains(&0) || value.contains(&0) || invalid_equals {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "environment contains an invalid name or NUL",
            ));
        }
        block.extend(name);
        block.push(b'=' as u16);
        block.extend(value);
        block.push(0);
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    Ok(block)
}

impl Child {
    #[cfg(test)]
    pub(super) fn waiter_id(&self) -> Option<tokio::task::Id> {
        self.waiter.as_ref().map(tokio::task::JoinHandle::id)
    }

    pub(super) fn take_stdout(&mut self) -> Option<ProcessPipe> {
        self.stdout.take()
    }

    pub(super) fn take_stderr(&mut self) -> Option<ProcessPipe> {
        self.stderr.take()
    }

    pub(super) fn killer(&self) -> Killer {
        self.killer.clone()
    }

    pub(super) async fn wait(&mut self) -> io::Result<ProcessExit> {
        if let Some(exit) = self.exit {
            return Ok(exit);
        }
        if self.waiter.is_none() {
            let wait_handle = self.process.duplicate(false)?;
            self.waiter = Some(tokio::task::spawn_blocking(move || {
                wait_process(&wait_handle)
            }));
        }
        let result = self.waiter.as_mut().expect("waiter was initialized").await;
        self.waiter = None;
        result.map_err(|error| io::Error::other(format!("process waiter failed: {error}")))??;
        let mut code = 0u32;
        if unsafe { GetExitCodeProcess(self.process.raw(), &mut code) } == 0 {
            return Err(last_error());
        }
        let exit = ProcessExit {
            code: Some(code as i32),
            success: code == 0,
        };
        self.exit = Some(exit);
        Ok(exit)
    }

    pub(super) async fn cleanup_after_exit(&mut self, timeout: Duration) -> io::Result<()> {
        if self.killer.is_alive()? {
            self.killer.terminate()?;
        }
        wait_tree_empty(&self.killer, timeout).await?;
        self.finish_debugger()
    }

    pub(super) async fn terminate_and_wait(
        &mut self,
        timeout: Duration,
    ) -> io::Result<ProcessExit> {
        self.killer.terminate()?;
        let exit = tokio::time::timeout(timeout, self.wait())
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "timed out reaping process root")
            })??;
        wait_tree_empty(&self.killer, timeout).await?;
        self.finish_debugger()?;
        Ok(exit)
    }

    fn finish_debugger(&mut self) -> io::Result<()> {
        match self.debugger.as_mut() {
            Some(debugger) => debugger.finish(),
            None => Ok(()),
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.killer.terminate();
    }
}

impl Killer {
    pub(super) fn terminate(&self) -> io::Result<()> {
        if self.jobs.terminated.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut first_error = None;
        for job in [&self.jobs.descendants, &self.jobs.root] {
            if unsafe { TerminateJobObject(job.handle.raw(), 1) } == 0 && first_error.is_none() {
                first_error = Some(last_error());
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        self.jobs.terminated.store(true, Ordering::Release);
        Ok(())
    }

    pub(super) fn is_alive(&self) -> io::Result<bool> {
        Ok(job_is_alive(&self.jobs.root)? || job_is_alive(&self.jobs.descendants)?)
    }
}

fn job_is_alive(job: &Job) -> io::Result<bool> {
    let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        QueryInformationJobObject(
            job.handle.raw(),
            JobObjectBasicAccountingInformation,
            (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error());
    }
    Ok(accounting.ActiveProcesses != 0)
}

fn wait_process(handle: &OwnedHandle) -> io::Result<()> {
    match unsafe { WaitForSingleObject(handle.raw(), INFINITE) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_FAILED => Err(last_error()),
        other => Err(io::Error::other(format!(
            "unexpected process wait result {other}"
        ))),
    }
}

async fn wait_tree_empty(killer: &Killer, timeout: Duration) -> io::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while killer.is_alive()? {
        if tokio::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Windows Job Object did not become empty",
            ));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

fn last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(value: &str) -> String {
        let mut encoded = Vec::new();
        append_quoted_arg(&mut encoded, OsStr::new(value), "test").unwrap();
        String::from_utf16(&encoded).unwrap()
    }

    #[test]
    fn windows_argv_quoting_handles_spaces_quotes_and_trailing_slashes() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote("a b"), "\"a b\"");
        assert_eq!(quote("a\\\"b"), "\"a\\\\\\\"b\"");
        assert_eq!(quote("a b\\"), "\"a b\\\\\"");
    }

    #[test]
    fn relative_executable_is_rejected_before_process_creation() {
        let spec = ProcessSpec::new("cmd.exe", std::env::current_dir().unwrap());
        let error = spawn(spec).err().expect("relative executable must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("absolute"));
    }

    #[test]
    fn environment_keys_use_windows_case_folding_without_lossy_conversion() {
        use std::os::windows::ffi::OsStringExt as _;

        let mixed = EnvKey::from(OsString::from("AnThRoPiC_ApI_KeY"));
        let lower = EnvKey::from(OsString::from("anthropic_api_key"));
        assert_eq!(mixed, lower);

        let surrogate = EnvKey::from(OsString::from_wide(&[0xd800]));
        let replacement = EnvKey::from(OsString::from_wide(&[0xfffd]));
        assert_ne!(surrogate, replacement);
    }
}
