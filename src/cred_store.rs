#[cfg(windows)]
mod windows {
    use windows_sys::core::PCWSTR;
    use windows_sys::w;
    use windows_sys::Win32::Foundation::{FILETIME, TRUE};
    use windows_sys::Win32::Security::Credentials::{
        CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    const TARGET_NAME: PCWSTR = w!("OneDriveSlideShow");

    pub fn get_refresh_token() -> Option<String> {
        let mut p_credential: *mut CREDENTIALW = std::ptr::null_mut() as *mut _;
        let bytes = unsafe {
            if CredReadW(
                TARGET_NAME,
                CRED_TYPE_GENERIC,
                0,
                &mut p_credential as *mut _,
            ) != TRUE
            {
                return None;
            }
            std::slice::from_raw_parts(
                (*p_credential).CredentialBlob,
                (*p_credential).CredentialBlobSize as usize,
            )
        };
        let token = String::from_utf8(bytes.to_vec()).map_err(Box::new);
        unsafe { CredFree(p_credential as *mut _) };
        token.ok()
    }

    pub fn store_refresh_token(cred: &str) {
        let credential = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: TARGET_NAME as *mut _,
            Comment: w!("OneDrive Slideshow Refresh Token") as *mut _,
            LastWritten: FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            },
            CredentialBlobSize: cred.len() as u32,
            CredentialBlob: cred.as_bytes().as_ptr() as *mut u8,
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: std::ptr::null_mut(),
            UserName: std::ptr::null_mut(),
        };
        unsafe {
            CredWriteW(&credential, 0);
        }
    }
}
#[cfg(windows)]
pub use windows::*;

#[cfg(not(windows))]
mod other {
    pub fn get_refresh_token() -> Option<String> {
        None
    }

    pub fn store_refresh_token(_cred: &str) {}
}
#[cfg(not(windows))]
pub use other::*;
