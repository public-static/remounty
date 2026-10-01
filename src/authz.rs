//! macOS Authorization Services.
//!
//! The app creates one authorization session and hands its external form
//! (an opaque 32-byte token) to the helper over a pipe. The helper then asks
//! macOS to authorize the request, which shows the standard system dialog.
//! Because the app keeps the same session alive, a "remember" timeout set on
//! the right only benefits Remounty; other programs cannot reuse it (the
//! rights are installed with `shared = false`).

use std::ffi::{CString, c_char, c_void};
use std::sync::Mutex;

use crate::error::{Error, Result};

type OSStatus = i32;
type AuthorizationRef = *mut c_void;

#[repr(C)]
struct AuthorizationItem {
    name: *const c_char,
    value_length: usize,
    value: *mut c_void,
    flags: u32,
}

#[repr(C)]
struct AuthorizationItemSet {
    count: u32,
    items: *mut AuthorizationItem,
}

pub const EXTERNAL_FORM_LEN: usize = 32;

#[repr(C)]
struct AuthorizationExternalForm {
    bytes: [u8; EXTERNAL_FORM_LEN],
}

const FLAG_DEFAULTS: u32 = 0;
const FLAG_INTERACTION_ALLOWED: u32 = 1 << 0;
const FLAG_EXTEND_RIGHTS: u32 = 1 << 1;

const ERR_SUCCESS: OSStatus = 0;
const ERR_DENIED: OSStatus = -60005;
const ERR_CANCELED: OSStatus = -60006;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn AuthorizationCreate(
        rights: *const AuthorizationItemSet,
        environment: *const AuthorizationItemSet,
        flags: u32,
        authorization: *mut AuthorizationRef,
    ) -> OSStatus;
    fn AuthorizationFree(authorization: AuthorizationRef, flags: u32) -> OSStatus;
    fn AuthorizationCopyRights(
        authorization: AuthorizationRef,
        rights: *const AuthorizationItemSet,
        environment: *const AuthorizationItemSet,
        flags: u32,
        authorized_rights: *mut *mut AuthorizationItemSet,
    ) -> OSStatus;
    fn AuthorizationMakeExternalForm(
        authorization: AuthorizationRef,
        external_form: *mut AuthorizationExternalForm,
    ) -> OSStatus;
    fn AuthorizationCreateFromExternalForm(
        external_form: *const AuthorizationExternalForm,
        authorization: *mut AuthorizationRef,
    ) -> OSStatus;
}

struct SessionRef(AuthorizationRef);
// SAFETY: an AuthorizationRef is an opaque handle that Authorization Services
// allows to be used from any thread; access is serialized by the mutex.
unsafe impl Send for SessionRef {}

static SESSION: Mutex<Option<SessionRef>> = Mutex::new(None);

/// App side: the external form of Remounty's authorization session,
/// creating the session on first use. It carries no rights by itself.
pub fn session_external_form() -> Result<[u8; EXTERNAL_FORM_LEN]> {
    let mut guard = SESSION
        .lock()
        .map_err(|_| Error::new("Authorization session lock poisoned"))?;
    if guard.is_none() {
        let mut authorization: AuthorizationRef = std::ptr::null_mut();
        // SAFETY: null rights/environment are allowed; the out pointer is valid.
        let status = unsafe {
            AuthorizationCreate(
                std::ptr::null(),
                std::ptr::null(),
                FLAG_DEFAULTS,
                &mut authorization,
            )
        };
        if status != ERR_SUCCESS || authorization.is_null() {
            return Err(Error::new(format!(
                "Could not create an authorization session ({status})"
            )));
        }
        *guard = Some(SessionRef(authorization));
    }
    let Some(session) = guard.as_ref() else {
        return Err(Error::new("Authorization session unavailable"));
    };
    let mut form = AuthorizationExternalForm {
        bytes: [0; EXTERNAL_FORM_LEN],
    };
    // SAFETY: `session.0` is a live AuthorizationRef; `form` is writable.
    let status = unsafe { AuthorizationMakeExternalForm(session.0, &mut form) };
    if status != ERR_SUCCESS {
        return Err(Error::new(format!(
            "Could not export the authorization session ({status})"
        )));
    }
    Ok(form.bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Granted,
    Canceled,
    Denied,
}

/// Helper side: asks macOS whether the session behind `external_form` holds
/// `right`, showing the system authentication dialog (with `prompt`) if the
/// user has to authenticate.
pub fn authorize(external_form: &[u8; EXTERNAL_FORM_LEN], right: &str, prompt: &str) -> Result<Decision> {
    let form = AuthorizationExternalForm {
        bytes: *external_form,
    };
    let mut authorization: AuthorizationRef = std::ptr::null_mut();
    // SAFETY: `form` is a valid external form buffer; the out pointer is valid.
    let status = unsafe { AuthorizationCreateFromExternalForm(&form, &mut authorization) };
    if status != ERR_SUCCESS || authorization.is_null() {
        return Err(Error::new(format!("Invalid authorization session ({status})")));
    }

    let right_c = CString::new(right).map_err(|_| Error::new("Invalid right name"))?;
    let prompt_c = CString::new(prompt.replace('\0', "")).map_err(|_| Error::new("Invalid prompt"))?;
    let prompt_key = CString::new("prompt").map_err(|_| Error::new("Invalid key"))?;

    let mut right_item = AuthorizationItem {
        name: right_c.as_ptr(),
        value_length: 0,
        value: std::ptr::null_mut(),
        flags: 0,
    };
    let rights = AuthorizationItemSet {
        count: 1,
        items: &mut right_item,
    };
    let mut prompt_item = AuthorizationItem {
        name: prompt_key.as_ptr(),
        value_length: prompt_c.as_bytes().len(),
        value: prompt_c.as_ptr() as *mut c_void,
        flags: 0,
    };
    let environment = AuthorizationItemSet {
        count: 1,
        items: &mut prompt_item,
    };

    // SAFETY: all pointers reference locals that outlive the call; a null
    // `authorized_rights` means we do not want the granted set back.
    let status = unsafe {
        AuthorizationCopyRights(
            authorization,
            &rights,
            &environment,
            FLAG_INTERACTION_ALLOWED | FLAG_EXTEND_RIGHTS,
            std::ptr::null_mut(),
        )
    };
    // Free without destroying the credentials, so a configured "remember"
    // period keeps working for the app's session.
    // SAFETY: `authorization` was created above and is freed exactly once.
    unsafe {
        AuthorizationFree(authorization, FLAG_DEFAULTS);
    }
    match status {
        ERR_SUCCESS => Ok(Decision::Granted),
        ERR_CANCELED => Ok(Decision::Canceled),
        ERR_DENIED => Ok(Decision::Denied),
        other => Err(Error::new(format!("Authorization failed ({other})"))),
    }
}
