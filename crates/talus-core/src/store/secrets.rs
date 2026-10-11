//! Credential storage. Production uses the macOS Keychain; tests use `MemoryStore`.

use std::collections::HashMap;
use std::sync::Mutex;

use uuid::Uuid;

use crate::secret::Secret;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretKind {
    DbPassword,
    /// SSH password / key passphrase or HTTP proxy password (Keychain suffix kept as "ssh").
    TunnelSecret,
}

impl SecretKind {
    fn suffix(self) -> &'static str {
        match self {
            SecretKind::DbPassword => "db",
            SecretKind::TunnelSecret => "ssh",
        }
    }
}

pub const KEYCHAIN_SERVICE: &str = "io.github.fallowlone.Talus";

fn account(id: &Uuid, kind: SecretKind) -> String {
    format!("{id}:{}", kind.suffix())
}

pub trait SecretStore: Send + Sync {
    fn get(&self, id: &Uuid, kind: SecretKind) -> Result<Option<Secret>, String>;
    fn set(&self, id: &Uuid, kind: SecretKind, value: &Secret) -> Result<(), String>;
    fn delete(&self, id: &Uuid, kind: SecretKind) -> Result<(), String>;
}

#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, Secret>>);

impl SecretStore for MemoryStore {
    fn get(&self, id: &Uuid, kind: SecretKind) -> Result<Option<Secret>, String> {
        Ok(self.0.lock().map_err(|e| e.to_string())?.get(&account(id, kind)).cloned())
    }
    fn set(&self, id: &Uuid, kind: SecretKind, value: &Secret) -> Result<(), String> {
        self.0.lock().map_err(|e| e.to_string())?.insert(account(id, kind), value.clone());
        Ok(())
    }
    fn delete(&self, id: &Uuid, kind: SecretKind) -> Result<(), String> {
        self.0.lock().map_err(|e| e.to_string())?.remove(&account(id, kind));
        Ok(())
    }
}

#[cfg(target_os = "macos")]
pub struct KeychainStore;

#[cfg(target_os = "macos")]
impl SecretStore for KeychainStore {
    fn get(&self, id: &Uuid, kind: SecretKind) -> Result<Option<Secret>, String> {
        use security_framework::passwords::get_generic_password;
        match get_generic_password(KEYCHAIN_SERVICE, &account(id, kind)) {
            Ok(bytes) => {
                let s = String::from_utf8(bytes).map_err(|_| "stored secret is not UTF-8".to_string())?;
                Ok(Some(Secret::new(s)))
            }
            // errSecItemNotFound
            Err(e) if e.code() == -25300 => Ok(None),
            Err(e) => Err(format!("Keychain read failed: {e}")),
        }
    }
    fn set(&self, id: &Uuid, kind: SecretKind, value: &Secret) -> Result<(), String> {
        security_framework::passwords::set_generic_password(
            KEYCHAIN_SERVICE,
            &account(id, kind),
            value.expose().as_bytes(),
        )
        .map_err(|e| format!("Keychain write failed: {e}"))
    }
    fn delete(&self, id: &Uuid, kind: SecretKind) -> Result<(), String> {
        match security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, &account(id, kind)) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == -25300 => Ok(()),
            Err(e) => Err(format!("Keychain delete failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_roundtrip() {
        let s = MemoryStore::default();
        let id = Uuid::new_v4();
        assert!(s.get(&id, SecretKind::DbPassword).unwrap().is_none());
        s.set(&id, SecretKind::DbPassword, &Secret::new("pw")).unwrap();
        assert_eq!(s.get(&id, SecretKind::DbPassword).unwrap().unwrap().expose(), "pw");
        assert!(s.get(&id, SecretKind::TunnelSecret).unwrap().is_none());
        s.delete(&id, SecretKind::DbPassword).unwrap();
        assert!(s.get(&id, SecretKind::DbPassword).unwrap().is_none());
    }

    /// Touches the real login Keychain; run explicitly with `--ignored`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn keychain_roundtrip() {
        let s = KeychainStore;
        let id = Uuid::new_v4();
        s.set(&id, SecretKind::DbPassword, &Secret::new("talus-test")).unwrap();
        assert_eq!(s.get(&id, SecretKind::DbPassword).unwrap().unwrap().expose(), "talus-test");
        s.delete(&id, SecretKind::DbPassword).unwrap();
        assert!(s.get(&id, SecretKind::DbPassword).unwrap().is_none());
    }
}
