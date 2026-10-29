use std::{
    fs::{self, File},
    io::{self, Write},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::Path,
    ptr,
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    Storage::FileSystem::*,
    System::Threading::*,
};

fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}
fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "unsafe ene owner, ACL, reparse point or hardlink",
    )
}
struct Local(*mut std::ffi::c_void);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn user_sid() -> io::Result<String> {
    unsafe {
        let mut token = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = std::os::windows::io::OwnedHandle::from_raw_handle(token);
        let mut size = 0;
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut size,
        );
        let mut data = vec![0_u8; size as usize];
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            data.as_mut_ptr().cast(),
            size,
            &mut size,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        sid_string(
            ptr::read_unaligned(data.as_ptr().cast::<TOKEN_USER>())
                .User
                .Sid,
        )
    }
}
unsafe fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let allocation = Local(text.cast());
    let mut len = 0;
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
    }
    let value = String::from_utf16(unsafe { std::slice::from_raw_parts(text, len) })
        .map_err(io::Error::other)?;
    drop(allocation);
    Ok(value)
}
fn descriptor(sddl: &str) -> io::Result<Local> {
    let mut sd = ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(sddl).as_ptr(),
            1,
            &mut sd,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Local(sd))
}
fn validate(file: &File, directory: bool, private: bool) -> io::Result<()> {
    unsafe {
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        if GetFileInformationByHandle(file.as_raw_handle(), &mut info) == 0 {
            return Err(io::Error::last_os_error());
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
            || !directory && info.nNumberOfLinks != 1
        {
            return Err(denied());
        }
        let mut owner = ptr::null_mut();
        let mut dacl = ptr::null_mut();
        let mut sd = ptr::null_mut();
        let err = GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut sd,
        );
        if err != 0 {
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        let _allocation = Local(sd);
        let user = user_sid()?;
        if owner.is_null() || sid_string(owner)? != user || dacl.is_null() {
            return Err(denied());
        }
        for i in 0..(*dacl).AceCount as u32 {
            let mut ace = ptr::null_mut();
            if GetAce(dacl, i, &mut ace) == 0 {
                return Err(io::Error::last_os_error());
            }
            let header = &*ace.cast::<ACE_HEADER>();
            // Unknown allow/object/callback ACEs are rejected rather than guessed.
            if header.AceFlags & INHERIT_ONLY_ACE as u8 != 0 || header.AceType == 1 {
                continue;
            }
            if header.AceType != 0 {
                return Err(denied());
            }
            let allowed = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            let sid = sid_string(ptr::addr_of!(allowed.SidStart).cast_mut().cast())?;
            let writes = FILE_WRITE_DATA
                | FILE_APPEND_DATA
                | FILE_WRITE_EA
                | FILE_WRITE_ATTRIBUTES
                | FILE_DELETE_CHILD
                | DELETE
                | WRITE_DAC
                | WRITE_OWNER
                | GENERIC_WRITE
                | GENERIC_ALL;
            if sid != user
                && sid != "S-1-5-18"
                && !(!private && sid == "S-1-5-32-544")
                && (private || allowed.Mask & writes != 0)
            {
                return Err(denied());
            }
        }
    }
    Ok(())
}
fn handle(
    path: &Path,
    directory: bool,
    access: u32,
    creation: u32,
    sd: Option<&Local>,
) -> io::Result<File> {
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.map_or(ptr::null_mut(), |sd| sd.0),
        bInheritHandle: 0,
    };
    let handle = unsafe {
        CreateFileW(
            wide(path).as_ptr(),
            access | READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            if sd.is_some() {
                &mut sa
            } else {
                ptr::null_mut()
            },
            creation,
            FILE_FLAG_OPEN_REPARSE_POINT
                | if directory {
                    FILE_FLAG_BACKUP_SEMANTICS
                } else {
                    0
                },
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}
fn reject_reparse_parents(path: &Path) -> io::Result<Vec<File>> {
    let mut guards = Vec::new();
    // Check ancestors as well as the final handle, including directory junctions.
    for parent in path.ancestors().skip(1) {
        if parent.as_os_str().is_empty() {
            continue;
        }
        let file = handle(parent, true, 0, OPEN_EXISTING, None)?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(denied());
        }
        guards.push(file);
    }
    Ok(guards)
}
pub fn open(path: &Path, directory: bool, private: bool) -> io::Result<File> {
    let _parents = reject_reparse_parents(path)?;
    let file = handle(
        path,
        directory,
        if directory { 0 } else { GENERIC_READ },
        OPEN_EXISTING,
        None,
    )?;
    validate(&file, directory, private)?;
    Ok(file)
}
pub fn check(path: &Path, directory: bool, private: bool) -> io::Result<()> {
    open(path, directory, private).map(drop)
}
pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    if path.exists() {
        return check(path, true, true);
    }
    let parent = path.parent().ok_or_else(denied)?;
    if !parent.exists() {
        create_private_dir_all(parent)?;
    }
    let _parents = reject_reparse_parents(path)?;
    let user = user_sid()?;
    let sd = descriptor(&format!("O:{user}D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;SY)"))?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    if unsafe { CreateDirectoryW(wide(path).as_ptr(), &sa) } == 0
        && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
    {
        return Err(io::Error::last_os_error());
    }
    check(path, true, true)
}
pub fn private_file(path: &Path) -> io::Result<File> {
    let _parent = open(path.parent().ok_or_else(denied)?, true, true)?;
    let user = user_sid()?;
    let sd = descriptor(&format!("O:{user}D:P(A;;FA;;;{user})(A;;FA;;;SY)"))?;
    let file = handle(
        path,
        false,
        GENERIC_READ | GENERIC_WRITE,
        OPEN_ALWAYS,
        Some(&sd),
    )?;
    validate(&file, false, true)?;
    Ok(file)
}
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(denied)?;
    create_private_dir_all(parent)?;
    let _parent = open(parent, true, true)?;
    if path.exists() {
        check(path, false, true)?;
    }
    let tmp = parent.join(format!(".ene-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = private_file(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if unsafe {
            MoveFileExW(
                wide(&tmp).as_ptr(),
                wide(path).as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        check(path, false, true)
    })();
    let _ = fs::remove_file(tmp);
    result
}
