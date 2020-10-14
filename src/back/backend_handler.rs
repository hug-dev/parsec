// Copyright 2019 Contributors to the Parsec project.
// SPDX-License-Identifier: Apache-2.0
//! Convert requests to calls to the underlying provider
//!
//! The backend handler embodies the last processing step from external request
//! to internal function call - parsing of the request body and conversion to a
//! native operation which is then passed to the provider.
use crate::authenticators::ApplicationName;
use crate::providers::Provide;
use derivative::Derivative;
use log::{error, trace};
use parsec_interface::operations::Convert;
use parsec_interface::operations::{NativeOperation, NativeResult};
use parsec_interface::requests::{
    request::RequestHeader, Request, Response, ResponseStatus, Result,
};
use parsec_interface::requests::{BodyType, ProviderID};
use std::io::{Error, ErrorKind};
use std::sync::Arc;

/// Back end handler component
///
/// Component responsible for unmarshalling requests, passing the operation
/// to the provider and marshalling the result.
///
/// It also provides assessment capabilities, letting the dispatcher know if
/// it can process a request.
#[derive(Derivative)]
#[derivative(Debug)]
pub struct BackEndHandler {
    // Send and Sync are required for Arc<FrontEndHandler> to be Send.
    #[derivative(Debug = "ignore")]
    provider: Arc<dyn Provide + Send + Sync>,
    #[derivative(Debug = "ignore")]
    converter: Box<dyn Convert + Send + Sync>,
    provider_id: ProviderID,
    content_type: BodyType,
    accept_type: BodyType,
}

impl BackEndHandler {
    /// Convert a request into a response, given the result of the operation.
    fn result_to_response(&self, result: NativeResult, request_hdr: RequestHeader) -> Response {
        let mut response = Response::from_request_header(request_hdr, ResponseStatus::Success);
        match self.converter.result_to_body(result) {
            Ok(body) => response.body = body,
            Err(status) => response.header.status = status,
        };
        response
    }

    /// Assess whether the backend handler-provider pair is capable of handling
    /// the request.
    ///
    /// # Errors
    /// - if the provider ID can not perform the type of operation, returns
    /// `ResponseStatus::PsaErrorNotSupported`
    /// - if the provider ID does not match, returns `ResponseStatus::WrongProviderID`
    /// - if the content type does not match, returns `ResponseStatus::ContentTypeNotSupported`
    /// - if the accept type does not match, returns `ResponseStatus::AcceptTypeNotSupported`
    pub fn is_capable(&self, request: &Request) -> Result<()> {
        let header = &request.header;

        if (self.provider_id == ProviderID::Core) != header.opcode.is_core() {
            error!("The request's operation is not compatible with the provider targeted.");
            return Err(ResponseStatus::PsaErrorNotSupported);
        }

        if header.provider != self.provider_id {
            Err(ResponseStatus::WrongProviderID)
        } else if header.content_type != self.content_type {
            Err(ResponseStatus::ContentTypeNotSupported)
        } else if header.accept_type != self.accept_type {
            Err(ResponseStatus::AcceptTypeNotSupported)
        } else {
            Ok(())
        }
    }

    /// Unmarshall the request body, pass the operation to the provider and marshall
    /// the result back.
    ///
    /// If any of the steps fails, a response containing an appropriate status code is
    /// returned.
    pub fn execute_request(&self, request: Request, app_name: Option<ApplicationName>) -> Response {
        trace!("execute_request ingress");
        let opcode = request.header.opcode;
        let header = request.header;

        macro_rules! unwrap_or_else_return {
            ($result:expr) => {
                match $result {
                    Ok(value) => value,
                    Err(status) => return Response::from_request_header(header, status),
                }
            };
        }

        match unwrap_or_else_return!(self.converter.body_to_operation(request.body, opcode)) {
            NativeOperation::ListProviders(op_list_providers) => {
                let result =
                    unwrap_or_else_return!(self.provider.list_providers(op_list_providers));
                trace!("list_providers egress");
                self.result_to_response(NativeResult::ListProviders(result), header)
            }
            NativeOperation::ListOpcodes(op_list_opcodes) => {
                let result = unwrap_or_else_return!(self.provider.list_opcodes(op_list_opcodes));
                trace!("list_opcodes egress");
                self.result_to_response(NativeResult::ListOpcodes(result), header)
            }
            NativeOperation::Ping(op_ping) => {
                let result = unwrap_or_else_return!(self.provider.ping(op_ping));
                trace!("ping egress");
                self.result_to_response(NativeResult::Ping(result), header)
            }
            NativeOperation::PsaGenerateKey(op_generate_key) => {
                let app_name =
                    unwrap_or_else_return!(app_name.ok_or(ResponseStatus::NotAuthenticated));
                let result = unwrap_or_else_return!(self
                    .provider
                    .psa_generate_key(app_name, op_generate_key));
                trace!("psa_generate_key egress");
                self.result_to_response(NativeResult::PsaGenerateKey(result), header)
            }
            NativeOperation::PsaImportKey(op_import_key) => {
                let app_name =
                    unwrap_or_else_return!(app_name.ok_or(ResponseStatus::NotAuthenticated));
                let result =
                    unwrap_or_else_return!(self.provider.psa_import_key(app_name, op_import_key));
                trace!("psa_import_key egress");
                self.result_to_response(NativeResult::PsaImportKey(result), header)
            }
            NativeOperation::PsaExportPublicKey(op_export_public_key) => {
                let app_name =
                    unwrap_or_else_return!(app_name.ok_or(ResponseStatus::NotAuthenticated));
                let result = unwrap_or_else_return!(self
                    .provider
                    .psa_export_public_key(app_name, op_export_public_key));
                trace!("psa_export_public_key egress");
                self.result_to_response(NativeResult::PsaExportPublicKey(result), header)
            }
            NativeOperation::PsaExportKey(op_export_key) => {
                let app_name =
                    unwrap_or_else_return!(app_name.ok_or(ResponseStatus::NotAuthenticated));
                let result =
                    unwrap_or_else_return!(self.provider.psa_export_key(app_name, op_export_key));
                trace!("psa_export_public_key egress");
                self.result_to_response(NativeResult::PsaExportKey(result), header)
            }
            NativeOperation::PsaDestroyKey(op_destroy_key) => {
                let app_name =
                    unwrap_or_else_return!(app_name.ok_or(ResponseStatus::NotAuthenticated));
                let result =
                    unwrap_or_else_return!(self.provider.psa_destroy_key(app_name, op_destroy_key));
                trace!("psa_destroy_key egress");
                self.result_to_response(NativeResult::PsaDestroyKey(result), header)
            }
            NativeOperation::PsaSignHash(op_sign_hash) => {
                let app_name =
                    unwrap_or_else_return!(app_name.ok_or(ResponseStatus::NotAuthenticated));
                let result =
                    unwrap_or_else_return!(self.provider.psa_sign_hash(app_name, op_sign_hash));
                trace!("psa_sign_hash egress");
                self.result_to_response(NativeResult::PsaSignHash(result), header)
            }
            NativeOperation::PsaVerifyHash(op_verify_hash) => {
                let app_name =
                    unwrap_or_else_return!(app_name.ok_or(ResponseStatus::NotAuthenticated));
                let result =
                    unwrap_or_else_return!(self.provider.psa_verify_hash(app_name, op_verify_hash));
                trace!("psa_verify_hash egress");
                self.result_to_response(NativeResult::PsaVerifyHash(result), header)
            }
            NativeOperation::PsaAsymmetricEncrypt(op_asymmetric_encrypt) => {
                let app_name =
                    unwrap_or_else_return!(app_name.ok_or(ResponseStatus::NotAuthenticated));
                let result = unwrap_or_else_return!(self
                    .provider
                    .psa_asymmetric_encrypt(app_name, op_asymmetric_encrypt));
                trace!("psa_asymmetric_encrypt_egress");
                self.result_to_response(NativeResult::PsaAsymmetricEncrypt(result), header)
            }
            NativeOperation::PsaAsymmetricDecrypt(op_asymmetric_decrypt) => {
                let app_name =
                    unwrap_or_else_return!(app_name.ok_or(ResponseStatus::NotAuthenticated));
                let result = unwrap_or_else_return!(self
                    .provider
                    .psa_asymmetric_decrypt(app_name, op_asymmetric_decrypt));
                trace!("psa_asymmetric_decrypt_egress");
                self.result_to_response(NativeResult::PsaAsymmetricDecrypt(result), header)
            }
            NativeOperation::PsaAeadEncrypt(op_aead_encrypt) => {
                let app_name =
                    unwrap_or_else_return!(app_name.ok_or(ResponseStatus::NotAuthenticated));
                let result = unwrap_or_else_return!(self
                    .provider
                    .psa_aead_encrypt(app_name, op_aead_encrypt));
                trace!("psa_aead_encrypt_egress");
                self.result_to_response(NativeResult::PsaAeadEncrypt(result), header)
            }
            NativeOperation::PsaAeadDecrypt(op_aead_decrypt) => {
                let app_name =
                    unwrap_or_else_return!(app_name.ok_or(ResponseStatus::NotAuthenticated));
                let result = unwrap_or_else_return!(self
                    .provider
                    .psa_aead_decrypt(app_name, op_aead_decrypt));
                trace!("psa_aead_decrypt_egress");
                self.result_to_response(NativeResult::PsaAeadDecrypt(result), header)
            }
            NativeOperation::ListAuthenticators(op_list_authenticators) => {
                let result = unwrap_or_else_return!(self
                    .provider
                    .list_authenticators(op_list_authenticators));
                trace!("list_authenticators egress");
                self.result_to_response(NativeResult::ListAuthenticators(result), header)
            }
            NativeOperation::ListKeys(op_list_keys) => {
                let app_name =
                    unwrap_or_else_return!(app_name.ok_or(ResponseStatus::NotAuthenticated));
                let result =
                    unwrap_or_else_return!(self.provider.list_keys(app_name, op_list_keys));
                trace!("list_keys egress");
                self.result_to_response(NativeResult::ListKeys(result), header)
            }
            NativeOperation::PsaHashCompute(op_hash_compute) => {
                let _app_name =
                    unwrap_or_else_return!(app_name.ok_or(ResponseStatus::NotAuthenticated));
                let result =
                    unwrap_or_else_return!(self.provider.psa_hash_compute(op_hash_compute));
                trace!("psa_aead_decrypt_egress");
                self.result_to_response(NativeResult::PsaHashCompute(result), header)
            }
            NativeOperation::PsaHashCompare(op_hash_compare) => {
                let _app_name =
                    unwrap_or_else_return!(app_name.ok_or(ResponseStatus::NotAuthenticated));
                let result =
                    unwrap_or_else_return!(self.provider.psa_hash_compare(op_hash_compare));
                trace!("psa_aead_decrypt_egress");
                self.result_to_response(NativeResult::PsaHashCompare(result), header)
            }
            NativeOperation::PsaRawKeyAgreement(op_raw_key_agreement) => {
                let app_name =
                    unwrap_or_else_return!(app_name.ok_or(ResponseStatus::NotAuthenticated));
                let result = unwrap_or_else_return!(self
                    .provider
                    .psa_raw_key_agreement(app_name, op_raw_key_agreement));
                trace!("psa_raw_key_agreement_egress");
                self.result_to_response(NativeResult::PsaRawKeyAgreement(result), header)
            }
            NativeOperation::PsaGenerateRandom(op_generate_random) => {
                let result =
                    unwrap_or_else_return!(self.provider.psa_generate_random(op_generate_random));
                trace!("psa_generate_random_egress");
                self.result_to_response(NativeResult::PsaGenerateRandom(result), header)
            }
        }
    }
}

/// Builder for `BackEndHandler`
#[derive(Default, Derivative)]
#[derivative(Debug)]
pub struct BackEndHandlerBuilder {
    #[derivative(Debug = "ignore")]
    provider: Option<Arc<dyn Provide + Send + Sync>>,
    #[derivative(Debug = "ignore")]
    converter: Option<Box<dyn Convert + Send + Sync>>,
    provider_id: Option<ProviderID>,
    content_type: Option<BodyType>,
    accept_type: Option<BodyType>,
}

impl BackEndHandlerBuilder {
    /// Create a new BackEndHandler builder
    pub fn new() -> BackEndHandlerBuilder {
        BackEndHandlerBuilder {
            provider: None,
            converter: None,
            provider_id: None,
            content_type: None,
            accept_type: None,
        }
    }

    /// Add a provider to the builder
    pub fn with_provider(mut self, provider: Arc<dyn Provide + Send + Sync>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Add a converter to the builder
    pub fn with_converter(mut self, converter: Box<dyn Convert + Send + Sync>) -> Self {
        self.converter = Some(converter);
        self
    }

    /// Set the ID of the BackEndHandler
    pub fn with_provider_id(mut self, provider_id: ProviderID) -> Self {
        self.provider_id = Some(provider_id);
        self
    }

    /// Set the content type that the BackEndHandler supports
    pub fn with_content_type(mut self, content_type: BodyType) -> Self {
        self.content_type = Some(content_type);
        self
    }

    /// Set the accept type that the BackEndHandler supports
    pub fn with_accept_type(mut self, accept_type: BodyType) -> Self {
        self.accept_type = Some(accept_type);
        self
    }

    /// Build into a BackEndHandler
    pub fn build(self) -> std::io::Result<BackEndHandler> {
        Ok(BackEndHandler {
            provider: self
                .provider
                .ok_or_else(|| Error::new(ErrorKind::InvalidData, "provider is missing"))?,
            converter: self
                .converter
                .ok_or_else(|| Error::new(ErrorKind::InvalidData, "converter is missing"))?,
            provider_id: self
                .provider_id
                .ok_or_else(|| Error::new(ErrorKind::InvalidData, "provider_id is missing"))?,
            content_type: self
                .content_type
                .ok_or_else(|| Error::new(ErrorKind::InvalidData, "content_type is missing"))?,
            accept_type: self
                .accept_type
                .ok_or_else(|| Error::new(ErrorKind::InvalidData, "accept_type is missing"))?,
        })
    }
}
