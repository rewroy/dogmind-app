//! API keys a person has pasted: in Windows Credential Manager, one entry to each, and in nothing this app writes. The settings say only that there is one.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Where keys are kept, as a trait so that a test never reaches the person's real store.
pub trait KeyStore: Send + Sync {
    fn read(&self, target: &str) -> Result<Option<String>, String>;
    fn write(&self, target: &str, key: &str) -> Result<(), String>;
    /// A key that is not there has been removed already.
    fn remove(&self, target: &str) -> Result<(), String>;
    /// Every key of this app, when it leaves this PC.
    fn remove_all(&self) -> Result<(), String>;
}

/// For tests, and for a debug build told to with `DOGMIND_TEST_KEYS`: nothing outlives the process.
#[derive(Default)]
pub struct MemoryKeys(Mutex<HashMap<String, String>>);

impl KeyStore for MemoryKeys {
    fn read(&self, target: &str) -> Result<Option<String>, String> {
        Ok(self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(target).cloned())
    }

    fn write(&self, target: &str, key: &str) -> Result<(), String> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(target.to_string(), key.to_string());
        Ok(())
    }

    fn remove(&self, target: &str) -> Result<(), String> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(target);
        Ok(())
    }

    fn remove_all(&self) -> Result<(), String> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
        Ok(())
    }
}

/// Windows Credential Manager, as generic credentials of this user on this machine.
#[cfg(not(test))]
pub struct WindowsCredentials;

#[cfg(not(test))]
impl WindowsCredentials {
    fn name(target: &str) -> Vec<u16> {
        format!("Dogmind/{target}").encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(not(test))]
impl KeyStore for WindowsCredentials {
    fn read(&self, target: &str) -> Result<Option<String>, String> {
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND};
        use windows_sys::Win32::Security::Credentials::{CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC};
        let name = Self::name(target);
        let mut found: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: the name ends in a zero, and what comes back is read within its stated size and given back to the system.
        unsafe {
            if CredReadW(name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut found) == 0 {
                let error = GetLastError();
                return if error == ERROR_NOT_FOUND { Ok(None) } else { Err(format!("Credential Manager: error {error}")) };
            }
            let blob = std::slice::from_raw_parts((*found).CredentialBlob, (*found).CredentialBlobSize as usize);
            let key = String::from_utf8_lossy(blob).into_owned();
            CredFree(found.cast());
            Ok(Some(key))
        }
    }

    fn write(&self, target: &str, key: &str) -> Result<(), String> {
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::Security::Credentials::{CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC};
        let mut name = Self::name(target);
        let mut user: Vec<u16> = "Dogmind".encode_utf16().chain(std::iter::once(0)).collect();
        let mut blob = key.as_bytes().to_vec();
        // SAFETY: all zeros is a valid value of this plain structure, and what it points at lives through the call.
        unsafe {
            let mut credential: CREDENTIALW = std::mem::zeroed();
            credential.Type = CRED_TYPE_GENERIC;
            credential.TargetName = name.as_mut_ptr();
            credential.UserName = user.as_mut_ptr();
            credential.CredentialBlob = blob.as_mut_ptr();
            credential.CredentialBlobSize = blob.len() as u32;
            credential.Persist = CRED_PERSIST_LOCAL_MACHINE;
            if CredWriteW(&credential, 0) == 0 {
                // (a policy can forbid it: "do not allow storage of passwords and credentials", 1312 and others)
                return Err(crate::lang::tf("Windows did not keep the key (Credential Manager, error {0}). A policy on this PC may forbid saved passwords: under More, read it from an environment variable instead.", &[&GetLastError()]));
            }
        }
        Ok(())
    }

    fn remove(&self, target: &str) -> Result<(), String> {
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND};
        use windows_sys::Win32::Security::Credentials::{CredDeleteW, CRED_TYPE_GENERIC};
        let name = Self::name(target);
        // SAFETY: the name ends in a zero.
        unsafe {
            if CredDeleteW(name.as_ptr(), CRED_TYPE_GENERIC, 0) == 0 {
                let error = GetLastError();
                if error != ERROR_NOT_FOUND {
                    return Err(format!("Credential Manager: error {error}"));
                }
            }
        }
        Ok(())
    }

    fn remove_all(&self) -> Result<(), String> {
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND};
        use windows_sys::Win32::Security::Credentials::{CredDeleteW, CredEnumerateW, CredFree, CREDENTIALW, CRED_TYPE_GENERIC};
        let filter: Vec<u16> = "Dogmind/*".encode_utf16().chain(std::iter::once(0)).collect();
        let mut count = 0;
        let mut list: *mut *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: the filter ends in a zero, and the list is read within the count given and handed back.
        unsafe {
            if CredEnumerateW(filter.as_ptr(), 0, &mut count, &mut list) == 0 {
                let error = GetLastError();
                return if error == ERROR_NOT_FOUND { Ok(()) } else { Err(format!("Credential Manager: error {error}")) };
            }
            // (the person is told it is all gone, so what stays is said)
            let mut stayed = 0;
            for at in 0..count as usize {
                if CredDeleteW((**list.add(at)).TargetName, CRED_TYPE_GENERIC, 0) == 0 && GetLastError() != ERROR_NOT_FOUND {
                    stayed += 1;
                }
            }
            CredFree(list.cast());
            if stayed > 0 {
                return Err(format!("Credential Manager: {stayed} of this app's keys could not be removed"));
            }
        }
        Ok(())
    }
}

/// The store of this run.
pub fn store() -> &'static dyn KeyStore {
    static STORE: OnceLock<Box<dyn KeyStore>> = OnceLock::new();
    STORE.get_or_init(choose).as_ref()
}

#[cfg(test)]
fn choose() -> Box<dyn KeyStore> {
    Box::new(MemoryKeys::default())
}

#[cfg(not(test))]
fn choose() -> Box<dyn KeyStore> {
    // (for walking the screens with a fake data folder: Credential Manager is not in that folder, and a release build cannot be told this)
    if cfg!(debug_assertions) && std::env::var_os("DOGMIND_TEST_KEYS").is_some() {
        return Box::new(MemoryKeys::default());
    }
    Box::new(WindowsCredentials)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_comes_back_until_it_is_removed() {
        let keys = MemoryKeys::default();
        assert_eq!(keys.read("a").unwrap(), None);
        keys.write("a", "secret").unwrap();
        assert_eq!(keys.read("a").unwrap().as_deref(), Some("secret"));
        keys.write("a", "other").unwrap();
        assert_eq!(keys.read("a").unwrap().as_deref(), Some("other"));
        keys.remove("a").unwrap();
        keys.remove("a").unwrap();
        assert_eq!(keys.read("a").unwrap(), None);
    }
}
