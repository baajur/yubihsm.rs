//! Commands supported by the `MockHsm`

use super::{object::Payload, state::State};
use crate::{
    algorithm::*,
    audit::{AuditCommand, AuditOption, AuditTag},
    client::*,
    command::{Code, Message},
    connector::ConnectionError,
    error::HsmErrorKind,
    object,
    response::{self, Response},
    serialization::deserialize,
    session::{
        self,
        close::CloseSessionResponse,
        create::{CreateSessionCommand, CreateSessionResponse},
    },
    Capability, WrapMessage, WrapNonce,
};
use hmac::{Hmac, Mac};
use rand_os::{rand_core::RngCore, OsRng};
use ring::signature::Ed25519KeyPair;
use sha2::Sha256;
use std::io::Cursor;
use subtle::ConstantTimeEq;
use untrusted;

/// Create a new HSM session
pub(crate) fn create_session(
    state: &mut State,
    cmd_message: &Message,
) -> Result<Vec<u8>, ConnectionError> {
    let cmd: CreateSessionCommand = deserialize(cmd_message.data.as_ref())
        .unwrap_or_else(|e| panic!("error parsing CreateSession command data: {:?}", e));

    let session = state.create_session(cmd.authentication_key_id, cmd.host_challenge);

    let mut response = CreateSessionResponse {
        card_challenge: *session.card_challenge(),
        card_cryptogram: session.card_cryptogram(),
    }
    .serialize();

    response.session_id = Some(session.id);
    Ok(response.into())
}

/// Authenticate an HSM session
pub(crate) fn authenticate_session(
    state: &mut State,
    command: &Message,
) -> Result<Vec<u8>, ConnectionError> {
    let session_id = command
        .session_id
        .unwrap_or_else(|| panic!("no session ID in command: {:?}", command.command_type));

    Ok(state
        .get_session(session_id)?
        .channel
        .verify_authenticate_session(command)
        .unwrap()
        .into())
}

/// Encrypted session messages
pub(crate) fn session_message(
    state: &mut State,
    encrypted_command: Message,
) -> Result<Vec<u8>, ConnectionError> {
    let session_id = encrypted_command.session_id.unwrap_or_else(|| {
        panic!(
            "no session ID in command: {:?}",
            encrypted_command.command_type
        )
    });

    let command = state
        .get_session(session_id)?
        .decrypt_command(encrypted_command);

    let response = match command.command_type {
        Code::BlinkDevice => BlinkDeviceResponse {}.serialize(),
        Code::CloseSession => return close_session(state, session_id),
        Code::DeleteObject => delete_object(state, &command.data),
        Code::DeviceInfo => device_info(),
        Code::Echo => echo(&command.data),
        Code::ExportWrapped => export_wrapped(state, &command.data),
        Code::GenerateAsymmetricKey => gen_asymmetric_key(state, &command.data),
        Code::GenerateHmacKey => gen_hmac_key(state, &command.data),
        Code::GenerateWrapKey => gen_wrap_key(state, &command.data),
        Code::GetLogEntries => get_log_entries(),
        Code::GetObjectInfo => get_object_info(state, &command.data),
        Code::GetOpaqueObject => get_opaque(state, &command.data),
        Code::GetOption => get_option(state, &command.data),
        Code::GetPseudoRandom => get_pseudo_random(state, &command.data),
        Code::GetPublicKey => get_public_key(state, &command.data),
        Code::SignHmac => sign_hmac(state, &command.data),
        Code::ImportWrapped => import_wrapped(state, &command.data),
        Code::ListObjects => list_objects(state, &command.data),
        Code::PutAsymmetricKey => put_asymmetric_key(state, &command.data),
        Code::PutAuthenticationKey => put_authentication_key(state, &command.data),
        Code::PutHmacKey => put_hmac_key(state, &command.data),
        Code::PutOpaqueObject => put_opaque(state, &command.data),
        Code::SetOption => put_option(state, &command.data),
        Code::PutWrapKey => put_wrap_key(state, &command.data),
        Code::ResetDevice => return Ok(reset_device(state, session_id)),
        Code::SetLogIndex => SetLogIndexResponse {}.serialize(),
        Code::SignEddsa => sign_eddsa(state, &command.data),
        Code::GetStorageInfo => get_storage_info(),
        Code::VerifyHmac => verify_hmac(state, &command.data),
        unsupported => panic!("unsupported command type: {:?}", unsupported),
    };

    Ok(state
        .get_session(session_id)?
        .encrypt_response(response)
        .into())
}

/// Close an active session
fn close_session(state: &mut State, session_id: session::Id) -> Result<Vec<u8>, ConnectionError> {
    let response = state
        .get_session(session_id)?
        .encrypt_response(CloseSessionResponse {}.serialize());

    state.close_session(session_id);
    Ok(response.into())
}

/// Delete an object
fn delete_object(state: &mut State, cmd_data: &[u8]) -> response::Message {
    let command: DeleteObjectCommand = deserialize(cmd_data)
        .unwrap_or_else(|e| panic!("error parsing Code::DeleteObject: {:?}", e));

    if state
        .objects
        .remove(command.object_id, command.object_type)
        .is_some()
    {
        DeleteObjectResponse {}.serialize()
    } else {
        debug!("no such object ID: {:?}", command.object_id);
        HsmErrorKind::ObjectNotFound.into()
    }
}

/// Generate a mock device information report
fn device_info() -> response::Message {
    DeviceInfoResponse {
        major_version: 2,
        minor_version: 0,
        build_version: 0,
        serial_number: 2_000_000,
        log_store_capacity: 62,
        log_store_used: 62,
        algorithms: vec![
            Algorithm::Rsa(RsaAlg::PKCS1_SHA1),
            Algorithm::Rsa(RsaAlg::PKCS1_SHA256),
            Algorithm::Rsa(RsaAlg::PKCS1_SHA384),
            Algorithm::Rsa(RsaAlg::PKCS1_SHA512),
            Algorithm::Rsa(RsaAlg::PSS_SHA1),
            Algorithm::Rsa(RsaAlg::PSS_SHA256),
            Algorithm::Rsa(RsaAlg::PSS_SHA384),
            Algorithm::Rsa(RsaAlg::PSS_SHA512),
            Algorithm::Asymmetric(AsymmetricAlg::RSA_2048),
            Algorithm::Asymmetric(AsymmetricAlg::RSA_3072),
            Algorithm::Asymmetric(AsymmetricAlg::RSA_4096),
            Algorithm::Asymmetric(AsymmetricAlg::EC_P256),
            Algorithm::Asymmetric(AsymmetricAlg::EC_P384),
            Algorithm::Asymmetric(AsymmetricAlg::EC_P521),
            Algorithm::Asymmetric(AsymmetricAlg::EC_K256),
            Algorithm::Asymmetric(AsymmetricAlg::EC_BP256),
            Algorithm::Asymmetric(AsymmetricAlg::EC_BP384),
            Algorithm::Asymmetric(AsymmetricAlg::EC_BP512),
            Algorithm::Hmac(HmacAlg::SHA1),
            Algorithm::Hmac(HmacAlg::SHA256),
            Algorithm::Hmac(HmacAlg::SHA384),
            Algorithm::Hmac(HmacAlg::SHA512),
            Algorithm::Ecdsa(EcdsaAlg::SHA1),
            Algorithm::Kex(KexAlg::ECDH),
            Algorithm::Rsa(RsaAlg::OAEP_SHA1),
            Algorithm::Rsa(RsaAlg::OAEP_SHA256),
            Algorithm::Rsa(RsaAlg::OAEP_SHA384),
            Algorithm::Rsa(RsaAlg::OAEP_SHA512),
            Algorithm::Wrap(WrapAlg::AES128_CCM),
            Algorithm::Opaque(OpaqueAlg::DATA),
            Algorithm::Opaque(OpaqueAlg::X509_CERTIFICATE),
            Algorithm::Mgf(MgfAlg::SHA1),
            Algorithm::Mgf(MgfAlg::SHA256),
            Algorithm::Mgf(MgfAlg::SHA384),
            Algorithm::Mgf(MgfAlg::SHA512),
            Algorithm::Template(TemplateAlg::SSH),
            Algorithm::YubicoOtp(YubicoOtpAlg::AES128),
            Algorithm::Auth(AuthenticationAlg::YUBICO_AES),
            Algorithm::YubicoOtp(YubicoOtpAlg::AES192),
            Algorithm::YubicoOtp(YubicoOtpAlg::AES256),
            Algorithm::Wrap(WrapAlg::AES192_CCM),
            Algorithm::Wrap(WrapAlg::AES256_CCM),
            Algorithm::Ecdsa(EcdsaAlg::SHA256),
            Algorithm::Ecdsa(EcdsaAlg::SHA384),
            Algorithm::Ecdsa(EcdsaAlg::SHA512),
            Algorithm::Asymmetric(AsymmetricAlg::Ed25519),
            Algorithm::Asymmetric(AsymmetricAlg::EC_P224),
        ],
    }
    .serialize()
}

/// Echo a message back to the host
fn echo(cmd_data: &[u8]) -> response::Message {
    EchoResponse(cmd_data.into()).serialize()
}

/// Export an object from the HSM in encrypted form
fn export_wrapped(state: &mut State, cmd_data: &[u8]) -> response::Message {
    let ExportWrappedCommand {
        wrap_key_id,
        object_type,
        object_id,
    } = deserialize(cmd_data)
        .unwrap_or_else(|e| panic!("error parsing Code::ExportWrapped: {:?}", e));

    let nonce = WrapNonce::generate();

    match state
        .objects
        .wrap(wrap_key_id, object_id, object_type, &nonce)
    {
        Ok(ciphertext) => ExportWrappedResponse(WrapMessage { nonce, ciphertext }).serialize(),
        Err(e) => {
            debug!("error wrapping object: {}", e);
            HsmErrorKind::InvalidCommand.into()
        }
    }
}

/// Generate a new random asymmetric key
fn gen_asymmetric_key(state: &mut State, cmd_data: &[u8]) -> response::Message {
    let GenAsymmetricKeyCommand(command) = deserialize(cmd_data)
        .unwrap_or_else(|e| panic!("error parsing Code::GenAsymmetricKey: {:?}", e));

    state.objects.generate(
        command.key_id,
        object::Type::AsymmetricKey,
        command.algorithm,
        command.label,
        command.capabilities,
        Capability::default(),
        command.domains,
    );

    GenAsymmetricKeyResponse {
        key_id: command.key_id,
    }
    .serialize()
}

/// Generate a new random HMAC key
fn gen_hmac_key(state: &mut State, cmd_data: &[u8]) -> response::Message {
    let GenHMACKeyCommand(command) =
        deserialize(cmd_data).unwrap_or_else(|e| panic!("error parsing Code::GenHMACKey: {:?}", e));

    state.objects.generate(
        command.key_id,
        object::Type::HmacKey,
        command.algorithm,
        command.label,
        command.capabilities,
        Capability::default(),
        command.domains,
    );

    GenHMACKeyResponse {
        key_id: command.key_id,
    }
    .serialize()
}

/// Generate a new random wrap (i.e. AES-CCM) key
fn gen_wrap_key(state: &mut State, cmd_data: &[u8]) -> response::Message {
    let GenWrapKeyCommand {
        params,
        delegated_capabilities,
    } = deserialize(cmd_data).unwrap_or_else(|e| panic!("error parsing Code::GenWrapKey: {:?}", e));

    state.objects.generate(
        params.key_id,
        object::Type::WrapKey,
        params.algorithm,
        params.label,
        params.capabilities,
        delegated_capabilities,
        params.domains,
    );

    GenWrapKeyResponse {
        key_id: params.key_id,
    }
    .serialize()
}

/// Get mock log information
fn get_log_entries() -> response::Message {
    // TODO: mimic the YubiHSM's actual audit log
    LogEntries {
        unlogged_boot_events: 0,
        unlogged_auth_events: 0,
        num_entries: 0,
        entries: vec![],
    }
    .serialize()
}

/// Get detailed info about a specific object
fn get_object_info(state: &State, cmd_data: &[u8]) -> response::Message {
    let command: GetObjectInfoCommand = deserialize(cmd_data)
        .unwrap_or_else(|e| panic!("error parsing Code::GetObjectInfo: {:?}", e));

    if let Some(obj) = state
        .objects
        .get(command.0.object_id, command.0.object_type)
    {
        GetObjectInfoResponse(obj.object_info.clone()).serialize()
    } else {
        debug!("no such object ID: {:?}", command.0.object_id);
        HsmErrorKind::ObjectNotFound.into()
    }
}

/// Get an opaque object (X.509 certificate or other data) stored in the HSM
fn get_opaque(state: &State, cmd_data: &[u8]) -> response::Message {
    let command: GetOpaqueCommand = deserialize(cmd_data)
        .unwrap_or_else(|e| panic!("error parsing Code::GetOpaqueObject: {:?}", e));

    if let Some(obj) = state.objects.get(command.object_id, object::Type::Opaque) {
        GetOpaqueResponse(obj.payload.as_ref().into()).serialize()
    } else {
        debug!("no such opaque object ID: {:?}", command.object_id);
        HsmErrorKind::ObjectNotFound.into()
    }
}

/// Get an auditing option
fn get_option(state: &State, cmd_data: &[u8]) -> response::Message {
    let command: GetOptionCommand = deserialize(cmd_data)
        .unwrap_or_else(|e| panic!("error parsing Code::GetOpaqueObject: {:?}", e));

    let results = match command.tag {
        AuditTag::Command => state.command_audit_options.serialize(),
        AuditTag::Force => vec![state.force_audit.to_u8()],
    };

    GetOptionResponse(results).serialize()
}

/// Get bytes of random data
fn get_pseudo_random(_state: &State, cmd_data: &[u8]) -> response::Message {
    let command: GetPseudoRandomCommand = deserialize(cmd_data)
        .unwrap_or_else(|e| panic!("error parsing Code::GetPseudoRandom: {:?}", e));

    let mut rng = OsRng::new().unwrap();
    let mut bytes = vec![0u8; command.bytes as usize];
    rng.fill_bytes(&mut bytes[..]);

    GetPseudoRandomResponse { bytes }.serialize()
}

/// Get the public key associated with a key in the HSM
fn get_public_key(state: &State, cmd_data: &[u8]) -> response::Message {
    let command: GetPubKeyCommand =
        deserialize(cmd_data).unwrap_or_else(|e| panic!("error parsing Code::GetPubKey: {:?}", e));

    if let Some(obj) = state
        .objects
        .get(command.key_id, object::Type::AsymmetricKey)
    {
        PublicKey {
            algorithm: obj.algorithm().asymmetric().unwrap(),
            bytes: obj.payload.public_key_bytes().unwrap(),
        }
        .serialize()
    } else {
        debug!("no such object ID: {:?}", command.key_id);
        HsmErrorKind::ObjectNotFound.into()
    }
}

/// Generate a mock storage status report
fn get_storage_info() -> response::Message {
    // TODO: model actual free storage
    GetStorageInfoResponse {
        total_records: 256,
        free_records: 256,
        total_pages: 1024,
        free_pages: 1024,
        page_size: 126,
    }
    .serialize()
}

/// Import an object encrypted under a wrap key into the HSM
fn import_wrapped(state: &mut State, cmd_data: &[u8]) -> response::Message {
    let ImportWrappedCommand {
        wrap_key_id,
        nonce,
        ciphertext,
    } = deserialize(cmd_data)
        .unwrap_or_else(|e| panic!("error parsing Code::ImportWrapped: {:?}", e));

    match state.objects.unwrap(wrap_key_id, &nonce, ciphertext) {
        Ok(obj) => ImportWrappedResponse {
            object_type: obj.object_type,
            object_id: obj.object_id,
        }
        .serialize(),
        Err(e) => {
            debug!("error unwrapping object: {}", e);
            HsmErrorKind::InvalidCommand.into()
        }
    }
}

/// List all objects presently accessible to a session
fn list_objects(state: &State, cmd_data: &[u8]) -> response::Message {
    let command: ListObjectsCommand = deserialize(cmd_data)
        .unwrap_or_else(|e| panic!("error parsing Code::ListObjects: {:?}", e));

    let len = command.0.len() as u64;
    let mut cursor = Cursor::new(command.0);
    let mut filters = vec![];

    while cursor.position() < len {
        filters.push(Filter::deserialize(&mut cursor).unwrap());
    }

    let list_entries = state
        .objects
        .iter()
        .filter(|(_, object)| {
            if filters.is_empty() {
                true
            } else {
                filters.iter().all(|filter| match filter {
                    Filter::Algorithm(alg) => object.info().algorithm == *alg,
                    Filter::Capabilities(caps) => object.info().capabilities.contains(*caps),
                    Filter::Domains(doms) => object.info().domains.contains(*doms),
                    Filter::Label(label) => object.info().label == *label,
                    Filter::Id(id) => object.info().object_id == *id,
                    Filter::Type(ty) => object.info().object_type == *ty,
                })
            }
        })
        .map(|(_, object)| ListObjectsEntry {
            object_id: object.object_info.object_id,
            object_type: object.object_info.object_type,
            sequence: object.object_info.sequence,
        })
        .collect();

    ListObjectsResponse(list_entries).serialize()
}

/// Put an existing asymmetric key into the HSM
fn put_asymmetric_key(state: &mut State, cmd_data: &[u8]) -> response::Message {
    let PutAsymmetricKeyCommand { params, data } = deserialize(cmd_data)
        .unwrap_or_else(|e| panic!("error parsing Code::PutAsymmetricKey: {:?}", e));

    state.objects.put(
        params.id,
        object::Type::AsymmetricKey,
        params.algorithm,
        params.label,
        params.capabilities,
        Capability::default(),
        params.domains,
        &data,
    );

    PutAsymmetricKeyResponse { key_id: params.id }.serialize()
}

/// Put a new authentication key into the HSM
fn put_authentication_key(state: &mut State, cmd_data: &[u8]) -> response::Message {
    let PutAuthenticationKeyCommand {
        params,
        delegated_capabilities,
        authentication_key,
    } = deserialize(cmd_data)
        .unwrap_or_else(|e| panic!("error parsing Code::PutAuthenticationKey: {:?}", e));

    state.objects.put(
        params.id,
        object::Type::AuthenticationKey,
        params.algorithm,
        params.label,
        params.capabilities,
        delegated_capabilities,
        params.domains,
        &authentication_key.0,
    );

    PutAuthenticationKeyResponse { key_id: params.id }.serialize()
}

/// Put a new HMAC key into the HSM
fn put_hmac_key(state: &mut State, cmd_data: &[u8]) -> response::Message {
    let PutHMACKeyCommand { params, hmac_key } =
        deserialize(cmd_data).unwrap_or_else(|e| panic!("error parsing Code::PutHMACKey: {:?}", e));

    state.objects.put(
        params.id,
        object::Type::HmacKey,
        params.algorithm,
        params.label,
        params.capabilities,
        Capability::default(),
        params.domains,
        &hmac_key,
    );

    PutHMACKeyResponse { key_id: params.id }.serialize()
}

/// Put an opaque object (X.509 cert or other data) into the HSM
fn put_opaque(state: &mut State, cmd_data: &[u8]) -> response::Message {
    let PutOpaqueCommand { params, data } = deserialize(cmd_data)
        .unwrap_or_else(|e| panic!("error parsing Code::PutOpaqueObject: {:?}", e));

    state.objects.put(
        params.id,
        object::Type::Opaque,
        params.algorithm,
        params.label,
        params.capabilities,
        Capability::default(),
        params.domains,
        &data,
    );

    PutOpaqueResponse {
        object_id: params.id,
    }
    .serialize()
}

/// Change an HSM auditing setting
fn put_option(state: &mut State, cmd_data: &[u8]) -> response::Message {
    let SetOptionCommand { tag, length, value } =
        deserialize(cmd_data).unwrap_or_else(|e| panic!("error parsing Code::PutOption: {:?}", e));

    match tag {
        AuditTag::Force => {
            assert_eq!(length, 1);
            state.force_audit = AuditOption::from_u8(value[0]).unwrap()
        }
        AuditTag::Command => {
            assert_eq!(length, 2);
            let audit_cmd: AuditCommand = deserialize(&value)
                .unwrap_or_else(|e| panic!("error parsing AuditCommand: {:?}", e));

            state
                .command_audit_options
                .put(audit_cmd.command_type(), audit_cmd.audit_option());
        }
    }

    PutOptionResponse {}.serialize()
}

/// Put an existing wrap (i.e. AES-CCM) key into the HSM
fn put_wrap_key(state: &mut State, cmd_data: &[u8]) -> response::Message {
    let PutWrapKeyCommand {
        params,
        delegated_capabilities,
        data,
    } = deserialize(cmd_data).unwrap_or_else(|e| panic!("error parsing Code::PutWrapKey: {:?}", e));

    state.objects.put(
        params.id,
        object::Type::WrapKey,
        params.algorithm,
        params.label,
        params.capabilities,
        delegated_capabilities,
        params.domains,
        &data,
    );

    PutWrapKeyResponse { key_id: params.id }.serialize()
}

/// Reset the MockHsm back to its default state
fn reset_device(state: &mut State, session_id: session::Id) -> Vec<u8> {
    let response = state
        .get_session(session_id)
        .unwrap()
        .encrypt_response(ResetDeviceResponse(0x01).serialize())
        .into();

    state.reset();
    response
}

/// Sign a message using the Ed25519 signature algorithm
fn sign_eddsa(state: &State, cmd_data: &[u8]) -> response::Message {
    let command: SignDataEddsaCommand = deserialize(cmd_data)
        .unwrap_or_else(|e| panic!("error parsing Code::SignDataEdDSA: {:?}", e));

    if let Some(obj) = state
        .objects
        .get(command.key_id, object::Type::AsymmetricKey)
    {
        if let Payload::Ed25519KeyPair(ref seed) = obj.payload {
            let keypair =
                Ed25519KeyPair::from_seed_unchecked(untrusted::Input::from(seed)).unwrap();

            let mut signature_bytes = [0u8; ED25519_SIGNATURE_SIZE];
            signature_bytes.copy_from_slice(keypair.sign(command.data.as_ref()).as_ref());

            Ed25519Signature(signature_bytes).serialize()
        } else {
            debug!("not an Ed25519 key: {:?}", obj.algorithm());
            HsmErrorKind::InvalidCommand.into()
        }
    } else {
        debug!("no such object ID: {:?}", command.key_id);
        HsmErrorKind::ObjectNotFound.into()
    }
}

/// Compute the HMAC tag for the given data
fn sign_hmac(state: &State, cmd_data: &[u8]) -> response::Message {
    let command: SignHmacCommand =
        deserialize(cmd_data).unwrap_or_else(|e| panic!("error parsing Code::HMACData: {:?}", e));

    if let Some(obj) = state.objects.get(command.key_id, object::Type::HmacKey) {
        if let Payload::HmacKey(alg, ref key) = obj.payload {
            assert_eq!(alg, HmacAlg::SHA256);
            let mut mac = Hmac::<Sha256>::new_varkey(key).unwrap();
            mac.input(&command.data);
            let tag = mac.result();
            HmacTag(tag.code().as_ref().into()).serialize()
        } else {
            debug!("not an HMAC key: {:?}", obj.algorithm());
            HsmErrorKind::InvalidCommand.into()
        }
    } else {
        debug!("no such object ID: {:?}", command.key_id);
        HsmErrorKind::ObjectNotFound.into()
    }
}

/// Verify the HMAC tag for the given data
fn verify_hmac(state: &State, cmd_data: &[u8]) -> response::Message {
    let command: VerifyHMACCommand =
        deserialize(cmd_data).unwrap_or_else(|e| panic!("error parsing Code::HMACData: {:?}", e));

    if let Some(obj) = state.objects.get(command.key_id, object::Type::HmacKey) {
        if let Payload::HmacKey(alg, ref key) = obj.payload {
            assert_eq!(alg, HmacAlg::SHA256);

            // Because of a quirk of our serde parser everything winds up in the tag field
            let data = command.tag.into_vec();

            let mut mac = Hmac::<Sha256>::new_varkey(key).unwrap();
            mac.input(&data[32..]);
            let tag = mac.result().code();
            let is_ok = tag.as_slice().ct_eq(&data[..32]).unwrap_u8();

            VerifyHMACResponse(is_ok).serialize()
        } else {
            debug!("not an HMAC key: {:?}", obj.algorithm());
            HsmErrorKind::InvalidCommand.into()
        }
    } else {
        debug!("no such object ID: {:?}", command.key_id);
        HsmErrorKind::ObjectNotFound.into()
    }
}
