// Copyright 2020 Contributors to the Parsec project.
// SPDX-License-Identifier: Apache-2.0
use super::LocalIdStore;
use super::{KeyInfo, Pkcs11Provider};
use crate::key_info_managers;
use crate::key_info_managers::{KeyTriple, ManageKeyInfo};
use log::{error, warn};
use parsec_interface::operations::psa_key_attributes::*;
use parsec_interface::requests::{ResponseStatus, Result};
use std::sync::RwLock;

impl Pkcs11Provider {
    // This function returns the `RWLocks` found on the `Pkcs11Provider`
    // in the order in which they should *always* be taken. Changing the order
    // of locking in one method can very easily result in deadlocking.
    pub(super) fn get_ordered_locks<'a>(
        &'a self,
    ) -> (
        &'a RwLock<dyn ManageKeyInfo + Send + Sync>,
        &'a RwLock<LocalIdStore>,
    ) {
        (&self.key_info_store, &self.local_ids)
    }

    /// Gets a key identifier and key attributes from the Key Info Manager.
    pub(super) fn get_key_info(&self, key_triple: &KeyTriple) -> Result<([u8; 4], Attributes)> {
        let locks = self.get_ordered_locks();
        let store_handle = locks.0.read().expect("Local ID lock poisoned");
        match store_handle.get(key_triple) {
            Ok(Some(key_info)) => {
                if key_info.id.len() == 4 {
                    let mut dst = [0; 4];
                    dst.copy_from_slice(&key_info.id);
                    Ok((dst, key_info.attributes))
                } else {
                    error!("Stored Key ID is not valid.");
                    Err(ResponseStatus::KeyInfoManagerError)
                }
            }
            Ok(None) => Err(ResponseStatus::PsaErrorDoesNotExist),
            Err(string) => Err(key_info_managers::to_response_status(string)),
        }
    }

    pub(super) fn create_key_id(
        &self,
        key_triple: KeyTriple,
        key_attributes: Attributes,
    ) -> Result<[u8; 4]> {
        let locks = self.get_ordered_locks();
        let mut store_handle = locks.0.write().expect("Key store lock poisoned");
        let mut local_ids_handle = locks.1.write().expect("Local ID lock poisoned");
        let mut key_id = rand::random::<[u8; 4]>();
        while local_ids_handle.contains(&key_id) {
            key_id = rand::random::<[u8; 4]>();
        }
        let key_info = KeyInfo {
            id: key_id.to_vec(),
            attributes: key_attributes,
        };
        match store_handle.insert(key_triple.clone(), key_info) {
            Ok(insert_option) => {
                if insert_option.is_some() {
                    if crate::utils::GlobalConfig::log_error_details() {
                        warn!("Overwriting Key triple mapping ({})", key_triple);
                    } else {
                        warn!("Overwriting Key triple mapping");
                    }
                }
                let _ = local_ids_handle.insert(key_id);
                Ok(key_id)
            }
            Err(string) => Err(key_info_managers::to_response_status(string)),
        }
    }

    pub(super) fn remove_key_id(&self, key_triple: &KeyTriple) -> Result<[u8; 4]> {
        let locks = self.get_ordered_locks();
        let mut store_handle = locks.0.write().expect("Key store lock poisoned");
        let mut local_ids_handle = locks.1.write().expect("Local ID lock poisoned");
        match store_handle.remove(key_triple) {
            Ok(Some(key_info)) => {
                let mut key_id = [0; 4];
                if key_info.id.len() == 4 {
                    key_id.copy_from_slice(&key_info.id);
                } else {
                    error!("Key info contained invalid key ID");
                    return Err(ResponseStatus::PsaErrorDataCorrupt);
                };
                let _ = local_ids_handle.remove(&key_id);
                Ok(key_id)
            }
            Ok(None) => {
                error!("Did not find expected key info.");
                Err(ResponseStatus::PsaErrorDoesNotExist)
            }
            Err(string) => Err(key_info_managers::to_response_status(string)),
        }
    }

    pub(super) fn key_info_exists(&self, key_triple: &KeyTriple) -> Result<bool> {
        let locks = self.get_ordered_locks();
        let store_handle = locks.0.read().expect("Key store lock poisoned");
        match store_handle.exists(key_triple) {
            Ok(val) => Ok(val),
            Err(string) => Err(key_info_managers::to_response_status(string)),
        }
    }
}
