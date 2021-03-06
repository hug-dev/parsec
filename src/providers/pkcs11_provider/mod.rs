// Copyright (c) 2019, Arm Limited, All Rights Reserved
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License"); you may
// not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//          http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use super::Provide;
use crate::authenticators::ApplicationName;
use crate::key_id_managers::{KeyTriple, ManageKeyIDs};
use log::{error, info, warn};
use parsec_interface::operations::key_attributes::*;
use parsec_interface::operations::ProviderInfo;
use parsec_interface::operations::{OpAsymSign, ResultAsymSign};
use parsec_interface::operations::{OpAsymVerify, ResultAsymVerify};
use parsec_interface::operations::{OpCreateKey, ResultCreateKey};
use parsec_interface::operations::{OpDestroyKey, ResultDestroyKey};
use parsec_interface::operations::{OpExportPublicKey, ResultExportPublicKey};
use parsec_interface::operations::{OpImportKey, ResultImportKey};
use parsec_interface::operations::{OpListOpcodes, ResultListOpcodes};
use parsec_interface::requests::{Opcode, ProviderID, ResponseStatus, Result};
use pkcs11::types::{
    CKF_OS_LOCKING_OK, CKF_RW_SESSION, CKF_SERIAL_SESSION, CKR_OK, CKR_SIGNATURE_INVALID, CKU_USER,
    CK_ATTRIBUTE, CK_C_INITIALIZE_ARGS, CK_MECHANISM, CK_OBJECT_HANDLE, CK_SESSION_HANDLE,
    CK_SLOT_ID,
};
use pkcs11::Ctx;
use serde::{Deserialize, Serialize};
use serde_asn1_der::asn1_wrapper::*;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};
use uuid::Uuid;
extern crate num_bigint_dig as num_bigint;
use num_bigint::{BigInt, Sign};

type LocalIdStore = HashSet<[u8; 4]>;

const SUPPORTED_OPCODES: [Opcode; 7] = [
    Opcode::CreateKey,
    Opcode::DestroyKey,
    Opcode::AsymSign,
    Opcode::AsymVerify,
    Opcode::ImportKey,
    Opcode::ExportPublicKey,
    Opcode::ListOpcodes,
];

// Public exponent value for all RSA keys.
const PUBLIC_EXPONENT: [u8; 3] = [0x01, 0x00, 0x01];

pub struct Pkcs11Provider {
    key_id_store: Arc<RwLock<dyn ManageKeyIDs + Send + Sync>>,
    // TODO: the local ID store is currently only used to prevent creating a key that does not
    // exist, it should also act as a cache for non-desctrucitve operations. Same for Mbed Crypto.
    local_ids: RwLock<LocalIdStore>,
    // The authentication state is common to all sessions. A counter of logged in sessions is used
    // to keep track of current logged in sessions, ignore logging in if the user is already
    // logged in and only log out when no other session is.
    // The mutex is both used to have interior mutability on the counter and to create a critical
    // section inside login/logout functions. The clippy warning is ignored here to not have one
    // Mutex<()> and an AtomicUsize which would make the code more complicated. Maybe a better
    // way exists.
    #[allow(clippy::mutex_atomic)]
    logged_sessions_counter: Mutex<usize>,
    backend: Ctx,
    slot_number: CK_SLOT_ID,
    // Some PKCS 11 devices do not need a pin, the None variant means that.
    user_pin: Option<String>,
}

// The RSA Public Key data are DER encoded with the following representation:
// RSAPublicKey ::= SEQUENCE {
//     modulus            INTEGER,  -- n
//     publicExponent     INTEGER   -- e
// }
#[derive(Serialize, Deserialize, Debug)]
struct RsaPublicKey {
    modulus: IntegerAsn1,
    public_exponent: IntegerAsn1,
}

// For PKCS 11, a key pair consists of two independant public and private keys. Both will share the
// same key ID.
enum KeyPairType {
    PublicKey,
    PrivateKey,
    Any,
}

// Representation of a PKCS 11 session.
struct Session<'a> {
    provider: &'a Pkcs11Provider,
    session_handle: CK_SESSION_HANDLE,
    // This information is necessary to log out when dropped.
    is_logged_in: bool,
}

#[derive(PartialEq)]
enum ReadWriteSession {
    ReadOnly,
    ReadWrite,
}

impl Session<'_> {
    fn new(provider: &Pkcs11Provider, read_write: ReadWriteSession) -> Result<Session> {
        info!("Opening session on slot {}", provider.slot_number);

        let mut session_flags = CKF_SERIAL_SESSION;
        if read_write == ReadWriteSession::ReadWrite {
            session_flags |= CKF_RW_SESSION;
        }

        match provider
            .backend
            .open_session(provider.slot_number, session_flags, None, None)
        {
            Ok(session_handle) => {
                let mut session = Session {
                    provider,
                    session_handle,
                    is_logged_in: false,
                };

                // The stress tests revealed bugs when sessions were concurrently running and some
                // of them where logging in and out during their execution. These bugs seemed to
                // disappear when *all* sessions are logged in by default.
                // See https://github.com/opendnssec/SoftHSMv2/issues/509 for reference.
                // This has security implications and should be disclosed.
                session.login()?;

                Ok(session)
            }
            Err(e) => {
                error!(
                    "Error opening session for slot {}: {}.",
                    provider.slot_number, e
                );
                Err(ResponseStatus::PsaErrorCommunicationFailure)
            }
        }
    }

    fn session_handle(&self) -> CK_SESSION_HANDLE {
        self.session_handle
    }

    fn login(&mut self) -> Result<()> {
        #[allow(clippy::mutex_atomic)]
        let mut logged_sessions_counter = self
            .provider
            .logged_sessions_counter
            .lock()
            .expect("Error while locking mutex.");

        if self.is_logged_in {
            info!(
                "This session ({}) has already requested authentication.",
                self.session_handle
            );
            Ok(())
        } else if *logged_sessions_counter > 0 {
            info!(
                "Logging in ignored as {} sessions are already requiring authentication.",
                *logged_sessions_counter
            );
            *logged_sessions_counter += 1;
            self.is_logged_in = true;
            Ok(())
        } else if let Some(user_pin) = self.provider.user_pin.as_ref() {
            match self
                .provider
                .backend
                .login(self.session_handle, CKU_USER, Some(user_pin))
            {
                Ok(_) => {
                    info!("Logging in session {}.", self.session_handle);
                    *logged_sessions_counter += 1;
                    self.is_logged_in = true;
                    Ok(())
                }
                Err(e) => {
                    error!("Login operation failed with {}", e);
                    Err(ResponseStatus::PsaErrorHardwareFailure)
                }
            }
        } else {
            warn!("Authentication requested but the provider has no user pin set!");
            Ok(())
        }
    }

    fn logout(&mut self) -> Result<()> {
        #[allow(clippy::mutex_atomic)]
        let mut logged_sessions_counter = self
            .provider
            .logged_sessions_counter
            .lock()
            .expect("Error while locking mutex.");

        if !self.is_logged_in {
            info!("Session {} has already logged out.", self.session_handle);
            Ok(())
        } else if *logged_sessions_counter == 0 {
            info!("The user is already logged out, ignoring.");
            Ok(())
        } else if *logged_sessions_counter == 1 {
            // Only this session requires authentication.
            match self.provider.backend.logout(self.session_handle) {
                Ok(_) => {
                    info!("Logged out in session {}.", self.session_handle);
                    *logged_sessions_counter -= 1;
                    self.is_logged_in = false;
                    Ok(())
                }
                Err(e) => {
                    error!(
                        "Failed to log out from session {} due to error {}. Continuing...",
                        self.session_handle, e
                    );
                    Err(ResponseStatus::PsaErrorHardwareFailure)
                }
            }
        } else {
            info!(
                "{} sessions are still requiring authentication, not logging out.",
                *logged_sessions_counter
            );
            *logged_sessions_counter -= 1;
            self.is_logged_in = false;
            Ok(())
        }
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        if self.logout().is_err() {
            error!("Error while logging out. Continuing...");
        }
        match self.provider.backend.close_session(self.session_handle) {
            Ok(_) => info!("Session {} closed.", self.session_handle),
            // Treat this as best effort.
            Err(e) => error!(
                "Failed to close session {} due to error {}. Continuing...",
                self.session_handle, e
            ),
        }
    }
}

/// Gets a key identifier from the Key ID Manager.
fn get_key_id(key_triple: &KeyTriple, store_handle: &dyn ManageKeyIDs) -> Result<[u8; 4]> {
    match store_handle.get(key_triple) {
        Ok(Some(key_id)) => {
            if key_id.len() == 4 {
                let mut dst = [0; 4];
                dst.copy_from_slice(key_id);
                Ok(dst)
            } else {
                error!("Stored Key ID is not valid.");
                Err(ResponseStatus::KeyIDManagerError)
            }
        }
        Ok(None) => Err(ResponseStatus::KeyDoesNotExist),
        Err(string) => {
            error!("Key ID Manager error: {}", string);
            Err(ResponseStatus::KeyIDManagerError)
        }
    }
}

fn create_key_id(
    key_triple: KeyTriple,
    store_handle: &mut dyn ManageKeyIDs,
    local_ids_handle: &mut LocalIdStore,
) -> Result<[u8; 4]> {
    let mut key_id = rand::random::<[u8; 4]>();
    while local_ids_handle.contains(&key_id) {
        key_id = rand::random::<[u8; 4]>();
    }
    match store_handle.insert(key_triple.clone(), key_id.to_vec()) {
        Ok(insert_option) => {
            if insert_option.is_some() {
                warn!("Overwriting Key triple mapping ({})", key_triple);
            }
            local_ids_handle.insert(key_id);

            Ok(key_id)
        }
        Err(string) => {
            error!("Key ID Manager error: {}", string);
            Err(ResponseStatus::KeyIDManagerError)
        }
    }
}

fn remove_key_id(
    key_triple: &KeyTriple,
    key_id: [u8; 4],
    store_handle: &mut dyn ManageKeyIDs,
    local_ids_handle: &mut LocalIdStore,
) -> Result<()> {
    match store_handle.remove(key_triple) {
        Ok(_) => {
            local_ids_handle.remove(&key_id);
            Ok(())
        }
        Err(string) => {
            error!("Key ID Manager error: {}", string);
            Err(ResponseStatus::KeyIDManagerError)
        }
    }
}

fn key_id_exists(key_triple: &KeyTriple, store_handle: &dyn ManageKeyIDs) -> Result<bool> {
    match store_handle.exists(key_triple) {
        Ok(val) => Ok(val),
        Err(string) => {
            error!("Key ID Manager error: {}", string);
            Err(ResponseStatus::KeyIDManagerError)
        }
    }
}

impl Pkcs11Provider {
    /// Creates and initialise a new instance of Pkcs11Provider.
    /// Checks if there are not more keys stored in the Key ID Manager than in the PKCS 11 library
    /// and if there are, delete them. Adds Key IDs currently in use in the local IDs store.
    /// Returns `None` if the initialisation failed.
    fn new(
        key_id_store: Arc<RwLock<dyn ManageKeyIDs + Send + Sync>>,
        backend: Ctx,
        slot_number: usize,
        user_pin: Option<String>,
    ) -> Option<Pkcs11Provider> {
        #[allow(clippy::mutex_atomic)]
        let pkcs11_provider = Pkcs11Provider {
            key_id_store,
            local_ids: RwLock::new(HashSet::new()),
            logged_sessions_counter: Mutex::new(0),
            backend,
            slot_number,
            user_pin,
        };
        {
            // The local scope allows to drop store_handle and local_ids_handle in order to return
            // the pkcs11_provider.
            let mut store_handle = pkcs11_provider
                .key_id_store
                .write()
                .expect("Key store lock poisoned");
            let mut local_ids_handle = pkcs11_provider
                .local_ids
                .write()
                .expect("Local ID lock poisoned");
            let mut to_remove: Vec<KeyTriple> = Vec::new();
            // Go through all PKCS 11 key triple to key ID mappings and check if they are still
            // present.
            // Delete those who are not present and add to the local_store the ones present.
            match store_handle.get_all(ProviderID::Pkcs11Provider) {
                Ok(key_triples) => {
                    let session =
                        Session::new(&pkcs11_provider, ReadWriteSession::ReadOnly).ok()?;

                    for key_triple in key_triples.iter().cloned() {
                        let key_id = match get_key_id(key_triple, &*store_handle) {
                            Ok(key_id) => key_id,
                            Err(response_status) => {
                                error!("Error getting the Key ID for triple:\n{}\n(error: {}), continuing...", key_triple, response_status);
                                continue;
                            }
                        };
                        match pkcs11_provider.find_key(
                            session.session_handle(),
                            key_id,
                            KeyPairType::Any,
                        ) {
                            Ok(_) => {
                                warn!(
                                    "Key {} found in the PKCS 11 library, adding it.",
                                    key_triple
                                );
                                local_ids_handle.insert(key_id);
                            }
                            Err(ResponseStatus::PsaErrorDoesNotExist) => {
                                warn!(
                                    "Key {} not found in the PKCS 11 library, deleting it.",
                                    key_triple
                                );
                                to_remove.push(key_triple.clone());
                            }
                            Err(e) => {
                                error!("Error finding key objects: {}.", e);
                                return None;
                            }
                        }
                    }
                }
                Err(string) => {
                    error!("Key ID Manager error: {}", string);
                    return None;
                }
            };
            for key_triple in to_remove.iter() {
                if let Err(string) = store_handle.remove(key_triple) {
                    error!("Key ID Manager error: {}", string);
                    return None;
                }
            }
        }

        Some(pkcs11_provider)
    }

    /// Find the PKCS 11 object handle corresponding to the key ID and the key type (public or
    /// private key) given as parameters for the current session.
    fn find_key(
        &self,
        session: CK_SESSION_HANDLE,
        key_id: [u8; 4],
        key_type: KeyPairType,
    ) -> Result<CK_OBJECT_HANDLE> {
        let mut template = vec![CK_ATTRIBUTE::new(pkcs11::types::CKA_ID).with_bytes(&key_id)];
        match key_type {
            KeyPairType::PublicKey => template.push(
                CK_ATTRIBUTE::new(pkcs11::types::CKA_CLASS)
                    .with_ck_ulong(&pkcs11::types::CKO_PUBLIC_KEY),
            ),
            KeyPairType::PrivateKey => template.push(
                CK_ATTRIBUTE::new(pkcs11::types::CKA_CLASS)
                    .with_ck_ulong(&pkcs11::types::CKO_PRIVATE_KEY),
            ),
            KeyPairType::Any => (),
        }

        if let Err(e) = self.backend.find_objects_init(session, &template) {
            error!("Object enumeration init failed with {}", e);
            Err(ResponseStatus::PsaErrorHardwareFailure)
        } else {
            match self.backend.find_objects(session, 1) {
                Ok(objects) => {
                    if let Err(e) = self.backend.find_objects_final(session) {
                        error!("Object enumeration final failed with {}", e);
                        Err(ResponseStatus::PsaErrorHardwareFailure)
                    } else if objects.is_empty() {
                        Err(ResponseStatus::PsaErrorDoesNotExist)
                    } else {
                        Ok(objects[0])
                    }
                }
                Err(e) => {
                    error!("Finding objects failed with {}", e);
                    Err(ResponseStatus::PsaErrorHardwareFailure)
                }
            }
        }
    }
}

impl Provide for Pkcs11Provider {
    fn list_opcodes(&self, _op: OpListOpcodes) -> Result<ResultListOpcodes> {
        Ok(ResultListOpcodes {
            opcodes: SUPPORTED_OPCODES.iter().copied().collect(),
        })
    }

    fn describe(&self) -> ProviderInfo {
        ProviderInfo {
            // Assigned UUID for this provider: 30e39502-eba6-4d60-a4af-c518b7f5e38f
            uuid: Uuid::parse_str("30e39502-eba6-4d60-a4af-c518b7f5e38f").unwrap(),
            description: String::from("PKCS #11 provider, interfacing with a PKCS #11 library."),
            vendor: String::from("OASIS Standard."),
            version_maj: 0,
            version_min: 1,
            version_rev: 0,
            id: ProviderID::Pkcs11Provider,
        }
    }

    fn create_key(&self, app_name: ApplicationName, op: OpCreateKey) -> Result<ResultCreateKey> {
        info!("Pkcs11 Provider - Create Key");

        if op.key_attributes.key_type != KeyType::RsaKeypair
            || op.key_attributes.algorithm != Algorithm::sign(SignAlgorithm::RsaPkcs1v15Sign, None)
        {
            error!("The PKCS 11 provider currently only supports creating RSA key pairs for signing and verifying.");
            return Err(ResponseStatus::UnsupportedOperation);
        }

        let key_name = op.key_name;
        // This should never panic on 32 bits or more machines.
        let key_size = std::convert::TryFrom::try_from(op.key_attributes.key_size).unwrap();

        let key_triple = KeyTriple::new(app_name, ProviderID::Pkcs11Provider, key_name);
        let mut store_handle = self.key_id_store.write().expect("Key store lock poisoned");
        let mut local_ids_handle = self.local_ids.write().expect("Local ID lock poisoned");
        if key_id_exists(&key_triple, &*store_handle)? {
            return Err(ResponseStatus::KeyAlreadyExists);
        }
        let key_id = create_key_id(
            key_triple.clone(),
            &mut *store_handle,
            &mut local_ids_handle,
        )?;

        let mech = CK_MECHANISM {
            mechanism: pkcs11::types::CKM_RSA_PKCS_KEY_PAIR_GEN,
            pParameter: std::ptr::null_mut(),
            ulParameterLen: 0,
        };

        let mut priv_template: Vec<CK_ATTRIBUTE> = Vec::new();
        let mut pub_template: Vec<CK_ATTRIBUTE> = Vec::new();

        priv_template
            .push(CK_ATTRIBUTE::new(pkcs11::types::CKA_SIGN).with_bool(&pkcs11::types::CK_TRUE));
        priv_template.push(CK_ATTRIBUTE::new(pkcs11::types::CKA_ID).with_bytes(&key_id));
        priv_template
            .push(CK_ATTRIBUTE::new(pkcs11::types::CKA_TOKEN).with_bool(&pkcs11::types::CK_TRUE));

        pub_template
            .push(CK_ATTRIBUTE::new(pkcs11::types::CKA_VERIFY).with_bool(&pkcs11::types::CK_TRUE));
        pub_template.push(CK_ATTRIBUTE::new(pkcs11::types::CKA_ID).with_bytes(&key_id));
        pub_template.push(
            CK_ATTRIBUTE::new(pkcs11::types::CKA_PUBLIC_EXPONENT).with_bytes(&PUBLIC_EXPONENT),
        );
        pub_template
            .push(CK_ATTRIBUTE::new(pkcs11::types::CKA_MODULUS_BITS).with_ck_ulong(&key_size));
        pub_template
            .push(CK_ATTRIBUTE::new(pkcs11::types::CKA_TOKEN).with_bool(&pkcs11::types::CK_TRUE));
        pub_template.push(
            CK_ATTRIBUTE::new(pkcs11::types::CKA_PRIVATE).with_bool(&pkcs11::types::CK_FALSE),
        );

        let session = Session::new(self, ReadWriteSession::ReadWrite).or_else(|err| {
            error!("Error creating a new session: {}.", err);
            remove_key_id(
                &key_triple,
                key_id,
                &mut *store_handle,
                &mut local_ids_handle,
            )?;
            Err(err)
        })?;

        info!(
            "Generating RSA key pair in session {}",
            session.session_handle()
        );

        match self.backend.generate_key_pair(
            session.session_handle(),
            &mech,
            &pub_template,
            &priv_template,
        ) {
            Ok(_key) => Ok(ResultCreateKey {}),
            Err(e) => {
                error!("Generate Key Pair operation failed with {}", e);
                remove_key_id(
                    &key_triple,
                    key_id,
                    &mut *store_handle,
                    &mut local_ids_handle,
                )?;
                Err(ResponseStatus::PsaErrorHardwareFailure)
            }
        }
    }

    fn import_key(&self, app_name: ApplicationName, op: OpImportKey) -> Result<ResultImportKey> {
        info!("Pkcs11 Provider - Import Key");

        if op.key_attributes.key_type != KeyType::RsaPublicKey
            || op.key_attributes.algorithm != Algorithm::sign(SignAlgorithm::RsaPkcs1v15Sign, None)
        {
            error!("The PKCS 11 provider currently only supports importing RSA public key for verifying.");
            return Err(ResponseStatus::UnsupportedOperation);
        }

        let key_name = op.key_name;
        let key_triple = KeyTriple::new(app_name, ProviderID::Pkcs11Provider, key_name);
        let mut store_handle = self.key_id_store.write().expect("Key store lock poisoned");
        let mut local_ids_handle = self.local_ids.write().expect("Local ID lock poisoned");
        if key_id_exists(&key_triple, &*store_handle)? {
            return Err(ResponseStatus::KeyAlreadyExists);
        }
        let key_id = create_key_id(
            key_triple.clone(),
            &mut *store_handle,
            &mut local_ids_handle,
        )?;

        let mut template: Vec<CK_ATTRIBUTE> = Vec::new();

        let public_key: RsaPublicKey = serde_asn1_der::from_bytes(&op.key_data).unwrap();

        let (_, modulus_object) = &public_key.modulus.to_bytes_be();
        let (_, exponent_object) = &public_key.public_exponent.to_bytes_be();

        template.push(
            CK_ATTRIBUTE::new(pkcs11::types::CKA_CLASS)
                .with_ck_ulong(&pkcs11::types::CKO_PUBLIC_KEY),
        );
        template.push(
            CK_ATTRIBUTE::new(pkcs11::types::CKA_KEY_TYPE).with_ck_ulong(&pkcs11::types::CKK_RSA),
        );
        template
            .push(CK_ATTRIBUTE::new(pkcs11::types::CKA_TOKEN).with_bool(&pkcs11::types::CK_TRUE));
        template.push(CK_ATTRIBUTE::new(pkcs11::types::CKA_MODULUS).with_bytes(modulus_object));
        template.push(
            CK_ATTRIBUTE::new(pkcs11::types::CKA_PUBLIC_EXPONENT).with_bytes(exponent_object),
        );
        template
            .push(CK_ATTRIBUTE::new(pkcs11::types::CKA_VERIFY).with_bool(&pkcs11::types::CK_TRUE));
        template.push(CK_ATTRIBUTE::new(pkcs11::types::CKA_ID).with_bytes(&key_id));

        let session = Session::new(self, ReadWriteSession::ReadWrite).or_else(|err| {
            error!("Error creating a new session: {}.", err);
            remove_key_id(
                &key_triple,
                key_id,
                &mut *store_handle,
                &mut local_ids_handle,
            )?;
            Err(err)
        })?;

        info!(
            "Importing RSA public key in session {}",
            session.session_handle()
        );

        match self
            .backend
            .create_object(session.session_handle(), &template)
        {
            Ok(_key) => Ok(ResultImportKey {}),
            Err(e) => {
                error!("Import operation failed with {}", e);
                remove_key_id(
                    &key_triple,
                    key_id,
                    &mut *store_handle,
                    &mut local_ids_handle,
                )?;
                Err(ResponseStatus::PsaErrorHardwareFailure)
            }
        }
    }

    fn export_public_key(
        &self,
        app_name: ApplicationName,
        op: OpExportPublicKey,
    ) -> Result<ResultExportPublicKey> {
        info!("Pkcs11 Provider - Export Public Key");

        let key_name = op.key_name;
        let key_triple = KeyTriple::new(app_name, ProviderID::Pkcs11Provider, key_name);
        let store_handle = self.key_id_store.read().expect("Key store lock poisoned");
        let key_id = get_key_id(&key_triple, &*store_handle)?;

        let session = Session::new(self, ReadWriteSession::ReadOnly)?;
        info!(
            "Export RSA public key in session {}",
            session.session_handle()
        );

        let key = self.find_key(session.session_handle(), key_id, KeyPairType::PublicKey)?;
        info!("Located key for export.");

        let mut size_attrs: Vec<CK_ATTRIBUTE> = Vec::new();
        size_attrs.push(CK_ATTRIBUTE::new(pkcs11::types::CKA_MODULUS));
        size_attrs.push(CK_ATTRIBUTE::new(pkcs11::types::CKA_PUBLIC_EXPONENT));

        // Get the length of the attributes to retrieve.
        let (modulus_len, public_exponent_len) =
            match self
                .backend
                .get_attribute_value(session.session_handle(), key, &mut size_attrs)
            {
                Ok((rv, attrs)) => {
                    if rv != CKR_OK {
                        error!("Error when extracting attribute: {}.", rv);
                        Err(ResponseStatus::PsaErrorCommunicationFailure)
                    } else {
                        Ok((attrs[0].ulValueLen, attrs[1].ulValueLen))
                    }
                }
                Err(e) => {
                    error!("Failed to read attributes from public key. Error: {}", e);
                    Err(ResponseStatus::PsaErrorCommunicationFailure)
                }
            }?;

        let mut modulus: Vec<pkcs11::types::CK_BYTE> = Vec::new();
        let mut public_exponent: Vec<pkcs11::types::CK_BYTE> = Vec::new();
        modulus.resize(modulus_len, 0);
        public_exponent.resize(public_exponent_len, 0);

        let mut extract_attrs: Vec<CK_ATTRIBUTE> = Vec::new();
        extract_attrs
            .push(CK_ATTRIBUTE::new(pkcs11::types::CKA_MODULUS).with_bytes(modulus.as_mut_slice()));
        extract_attrs.push(
            CK_ATTRIBUTE::new(pkcs11::types::CKA_PUBLIC_EXPONENT)
                .with_bytes(public_exponent.as_mut_slice()),
        );

        match self
            .backend
            .get_attribute_value(session.session_handle(), key, &mut extract_attrs)
        {
            Ok(res) => {
                let (rv, attrs) = res;
                if rv != CKR_OK {
                    error!("Error when extracting attribute: {}.", rv);
                    Err(ResponseStatus::PsaErrorCommunicationFailure)
                } else {
                    let key = RsaPublicKey {
                        modulus: IntegerAsn1(BigInt::from_bytes_be(
                            Sign::Plus,
                            &attrs[0].get_bytes(),
                        )),
                        public_exponent: IntegerAsn1(BigInt::from_bytes_be(
                            Sign::Plus,
                            &attrs[1].get_bytes(),
                        )),
                    };
                    let key_data = serde_asn1_der::to_vec(&key).or_else(|err| {
                        error!("Could not serialise key elements: {}.", err);
                        Err(ResponseStatus::PsaErrorCommunicationFailure)
                    })?;
                    Ok(ResultExportPublicKey { key_data })
                }
            }
            Err(e) => {
                error!("Failed to read attributes from public key. Error: {}", e);
                Err(ResponseStatus::PsaErrorCommunicationFailure)
            }
        }
    }

    fn destroy_key(&self, app_name: ApplicationName, op: OpDestroyKey) -> Result<ResultDestroyKey> {
        info!("Pkcs11 Provider - Destroy Key");

        let key_name = op.key_name;
        let key_triple = KeyTriple::new(app_name, ProviderID::Pkcs11Provider, key_name);
        let mut store_handle = self.key_id_store.write().expect("Key store lock poisoned");
        let mut local_ids_handle = self.local_ids.write().expect("Local ID lock poisoned");
        let key_id = get_key_id(&key_triple, &*store_handle)?;

        let session = Session::new(self, ReadWriteSession::ReadWrite)?;
        info!(
            "Deleting RSA keypair in session {}",
            session.session_handle()
        );

        match self.find_key(session.session_handle(), key_id, KeyPairType::Any) {
            Ok(key) => {
                match self.backend.destroy_object(session.session_handle(), key) {
                    Ok(_) => info!("Private part of the key destroyed successfully."),
                    Err(e) => {
                        error!("Failed to destroy private part of the key. Error: {}", e);
                        return Err(ResponseStatus::PsaErrorGenericError);
                    }
                };
            }
            Err(e) => {
                error!("Error destroying key: {}", e);
                return Err(e);
            }
        };

        // Second key is optional.
        match self.find_key(session.session_handle(), key_id, KeyPairType::Any) {
            Ok(key) => {
                match self.backend.destroy_object(session.session_handle(), key) {
                    Ok(_) => info!("Private part of the key destroyed successfully."),
                    Err(e) => {
                        error!("Failed to destroy private part of the key. Error: {}", e);
                        return Err(ResponseStatus::PsaErrorGenericError);
                    }
                };
            }
            // A second key is optional.
            Err(ResponseStatus::PsaErrorDoesNotExist) => (),
            Err(e) => {
                error!("Error destroying key: {}", e);
                return Err(e);
            }
        };

        remove_key_id(
            &key_triple,
            key_id,
            &mut *store_handle,
            &mut local_ids_handle,
        )?;

        Ok(ResultDestroyKey {})
    }

    fn asym_sign(&self, app_name: ApplicationName, op: OpAsymSign) -> Result<ResultAsymSign> {
        info!("Pkcs11 Provider - Asym Sign");

        let key_name = op.key_name;
        let hash = op.hash;
        let key_triple = KeyTriple::new(app_name, ProviderID::Pkcs11Provider, key_name);
        let store_handle = self.key_id_store.read().expect("Key store lock poisoned");
        let key_id = get_key_id(&key_triple, &*store_handle)?;

        let mech = pkcs11::types::CK_MECHANISM {
            mechanism: pkcs11::types::CKM_RSA_PKCS,
            pParameter: std::ptr::null_mut(),
            ulParameterLen: 0,
        };

        let session = Session::new(self, ReadWriteSession::ReadWrite)?;
        info!("Asymmetric sign in session {}", session.session_handle());

        let key = self.find_key(session.session_handle(), key_id, KeyPairType::PrivateKey)?;
        info!("Located signing key.");

        match self.backend.sign_init(session.session_handle(), &mech, key) {
            Ok(_) => {
                info!("Signing operation initialized.");
                match self.backend.sign(session.session_handle(), &hash) {
                    Ok(signature) => Ok(ResultAsymSign { signature }),
                    Err(e) => {
                        error!("Failed to execute signing operation. Error: {}", e);
                        Err(ResponseStatus::PsaErrorGenericError)
                    }
                }
            }
            Err(e) => {
                error!("Failed to initialize signing operation. Error: {}", e);
                Err(ResponseStatus::PsaErrorGenericError)
            }
        }
    }

    fn asym_verify(&self, app_name: ApplicationName, op: OpAsymVerify) -> Result<ResultAsymVerify> {
        info!("Pkcs11 Provider - Asym Verify");

        let key_name = op.key_name;
        let hash = op.hash;
        let signature = op.signature;
        let key_triple = KeyTriple::new(app_name, ProviderID::Pkcs11Provider, key_name);
        let store_handle = self.key_id_store.read().expect("Key store lock poisoned");
        let key_id = get_key_id(&key_triple, &*store_handle)?;

        let mech = pkcs11::types::CK_MECHANISM {
            mechanism: pkcs11::types::CKM_RSA_PKCS,
            pParameter: std::ptr::null_mut(),
            ulParameterLen: 0,
        };

        let session = Session::new(self, ReadWriteSession::ReadWrite)?;
        info!("Asymmetric verify in session {}", session.session_handle());

        let key = self.find_key(session.session_handle(), key_id, KeyPairType::PublicKey)?;
        info!("Located public key.");

        match self
            .backend
            .verify_init(session.session_handle(), &mech, key)
        {
            Ok(_) => {
                info!("Verify operation initialized.");
                match self
                    .backend
                    .verify(session.session_handle(), &hash, &signature)
                {
                    Ok(_) => Ok(ResultAsymVerify {}),
                    Err(e) => match e {
                        pkcs11::errors::Error::Pkcs11(CKR_SIGNATURE_INVALID) => {
                            info!("Signature verification failed.");
                            Err(ResponseStatus::PsaErrorInvalidSignature)
                        }
                        err => {
                            error!("Failed to execute verify operation. Error: {}", err);
                            Err(ResponseStatus::PsaErrorGenericError)
                        }
                    },
                }
            }
            Err(e) => {
                error!("Failed to initialize signing operation. Error: {}", e);
                Err(ResponseStatus::PsaErrorGenericError)
            }
        }
    }
}

#[derive(Default)]
pub struct Pkcs11ProviderBuilder {
    key_id_store: Option<Arc<RwLock<dyn ManageKeyIDs + Send + Sync>>>,
    pkcs11_library_path: Option<String>,
    slot_number: Option<usize>,
    user_pin: Option<String>,
}

impl Pkcs11ProviderBuilder {
    pub fn new() -> Pkcs11ProviderBuilder {
        Pkcs11ProviderBuilder {
            key_id_store: None,
            pkcs11_library_path: None,
            slot_number: None,
            user_pin: None,
        }
    }

    pub fn with_key_id_store(
        mut self,
        key_id_store: Arc<RwLock<dyn ManageKeyIDs + Send + Sync>>,
    ) -> Pkcs11ProviderBuilder {
        self.key_id_store = Some(key_id_store);

        self
    }

    pub fn with_pkcs11_library_path(
        mut self,
        pkcs11_library_path: String,
    ) -> Pkcs11ProviderBuilder {
        self.pkcs11_library_path = Some(pkcs11_library_path);

        self
    }

    pub fn with_slot_number(mut self, slot_number: usize) -> Pkcs11ProviderBuilder {
        self.slot_number = Some(slot_number);

        self
    }

    pub fn with_user_pin(mut self, user_pin: Option<String>) -> Pkcs11ProviderBuilder {
        self.user_pin = user_pin;

        self
    }

    pub fn build(self) -> Pkcs11Provider {
        let library_path = self
            .pkcs11_library_path
            .expect("Missing PKCS 11 library path");
        info!(
            "Building a PKCS 11 provider with library \'{}\'",
            library_path
        );
        let slot_number = self
            .slot_number
            .expect("The slot number of the device is needed to communicate with PKCS 11 library.");
        let mut backend = Ctx::new(library_path).unwrap();
        let mut args = CK_C_INITIALIZE_ARGS::new();
        // Allow the PKCS 11 library to use OS native locking mechanism.
        args.CreateMutex = None;
        args.DestroyMutex = None;
        args.LockMutex = None;
        args.UnlockMutex = None;
        args.flags = CKF_OS_LOCKING_OK;
        backend.initialize(Some(args)).unwrap();
        Pkcs11Provider::new(
            self.key_id_store.expect("Missing key ID store"),
            backend,
            slot_number,
            self.user_pin,
        )
        .expect("Failed to initialise PKCS 11 Provider")
    }
}
