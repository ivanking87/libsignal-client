//
// Copyright 2020-2021 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use crate::proto;
use crate::{IdentityKey, PrivateKey, PublicKey, Result, SignalProtocolError};

use std::convert::TryFrom;

use hmac::{Hmac, Mac, NewMac};
use prost::Message;
use rand::{CryptoRng, Rng};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use uuid::Uuid;

pub const CIPHERTEXT_MESSAGE_CURRENT_VERSION: u8 = 3;
pub const SENDERKEY_MESSAGE_CURRENT_VERSION: u8 = 3;

pub enum CiphertextMessage {
    SignalMessage(SignalMessage),
    PreKeySignalMessage(PreKeySignalMessage),
    SenderKeyMessage(SenderKeyMessage),
    PlaintextContent(PlaintextContent),
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, num_enum::TryFromPrimitive)]
#[repr(u8)]
pub enum CiphertextMessageType {
    Whisper = 2,
    PreKey = 3,
    SenderKey = 7,
    Plaintext = 8,
}

impl CiphertextMessage {
    pub fn message_type(&self) -> CiphertextMessageType {
        match self {
            CiphertextMessage::SignalMessage(_) => CiphertextMessageType::Whisper,
            CiphertextMessage::PreKeySignalMessage(_) => CiphertextMessageType::PreKey,
            CiphertextMessage::SenderKeyMessage(_) => CiphertextMessageType::SenderKey,
            CiphertextMessage::PlaintextContent(_) => CiphertextMessageType::Plaintext,
        }
    }

    pub fn serialize(&self) -> &[u8] {
        match self {
            CiphertextMessage::SignalMessage(x) => x.serialized(),
            CiphertextMessage::PreKeySignalMessage(x) => x.serialized(),
            CiphertextMessage::SenderKeyMessage(x) => x.serialized(),
            CiphertextMessage::PlaintextContent(x) => x.serialized(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SignalMessage {
    message_version: u8,
    sender_ratchet_key: PublicKey,
    counter: u32,
    #[allow(dead_code)]
    previous_counter: u32,
    ciphertext: Box<[u8]>,
    serialized: Box<[u8]>,
}

impl SignalMessage {
    const MAC_LENGTH: usize = 8;

    pub fn new(
        message_version: u8,
        mac_key: &[u8],
        sender_ratchet_key: PublicKey,
        counter: u32,
        previous_counter: u32,
        ciphertext: &[u8],
        sender_identity_key: &IdentityKey,
        receiver_identity_key: &IdentityKey,
    ) -> Result<Self> {
        let message = proto::wire::SignalMessage {
            ratchet_key: Some(sender_ratchet_key.serialize().into_vec()),
            counter: Some(counter),
            previous_counter: Some(previous_counter),
            ciphertext: Some(Vec::<u8>::from(ciphertext)),
        };
        let mut serialized = vec![0u8; 1 + message.encoded_len() + Self::MAC_LENGTH];
        serialized[0] = ((message_version & 0xF) << 4) | CIPHERTEXT_MESSAGE_CURRENT_VERSION;
        message.encode(&mut &mut serialized[1..message.encoded_len() + 1])?;
        let msg_len_for_mac = serialized.len() - Self::MAC_LENGTH;
        let mac = Self::compute_mac(
            sender_identity_key,
            receiver_identity_key,
            mac_key,
            &serialized[..msg_len_for_mac],
        )?;
        serialized[msg_len_for_mac..].copy_from_slice(&mac);
        let serialized = serialized.into_boxed_slice();
        Ok(Self {
            message_version,
            sender_ratchet_key,
            counter,
            previous_counter,
            ciphertext: ciphertext.into(),
            serialized,
        })
    }

    #[inline]
    pub fn message_version(&self) -> u8 {
        self.message_version
    }

    #[inline]
    pub fn sender_ratchet_key(&self) -> &PublicKey {
        &self.sender_ratchet_key
    }

    #[inline]
    pub fn counter(&self) -> u32 {
        self.counter
    }

    #[inline]
    pub fn serialized(&self) -> &[u8] {
        &*self.serialized
    }

    #[inline]
    pub fn body(&self) -> &[u8] {
        &*self.ciphertext
    }

    pub fn verify_mac(
        &self,
        sender_identity_key: &IdentityKey,
        receiver_identity_key: &IdentityKey,
        mac_key: &[u8],
    ) -> Result<bool> {
        let our_mac = &Self::compute_mac(
            sender_identity_key,
            receiver_identity_key,
            mac_key,
            &self.serialized[..self.serialized.len() - Self::MAC_LENGTH],
        )?;
        let their_mac = &self.serialized[self.serialized.len() - Self::MAC_LENGTH..];
        let result: bool = our_mac.ct_eq(their_mac).into();
        if !result {
            // A warning instead of an error because we try multiple sessions.
            log::warn!(
                "Bad Mac! Their Mac: {} Our Mac: {}",
                hex::encode(their_mac),
                hex::encode(our_mac)
            );
        }
        Ok(result)
    }

    fn compute_mac(
        sender_identity_key: &IdentityKey,
        receiver_identity_key: &IdentityKey,
        mac_key: &[u8],
        message: &[u8],
    ) -> Result<[u8; Self::MAC_LENGTH]> {
        if mac_key.len() != 32 {
            return Err(SignalProtocolError::InvalidMacKeyLength(mac_key.len()));
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(mac_key)
            .expect("HMAC-SHA256 should accept any size key");

        mac.update(sender_identity_key.public_key().serialize().as_ref());
        mac.update(receiver_identity_key.public_key().serialize().as_ref());
        mac.update(message);
        let mut result = [0u8; Self::MAC_LENGTH];
        result.copy_from_slice(&mac.finalize().into_bytes()[..Self::MAC_LENGTH]);
        Ok(result)
    }
}

impl AsRef<[u8]> for SignalMessage {
    fn as_ref(&self) -> &[u8] {
        &*self.serialized
    }
}

impl TryFrom<&[u8]> for SignalMessage {
    type Error = SignalProtocolError;

    fn try_from(value: &[u8]) -> Result<Self> {
        if value.len() < SignalMessage::MAC_LENGTH + 1 {
            return Err(SignalProtocolError::CiphertextMessageTooShort(value.len()));
        }
        let message_version = value[0] >> 4;
        if message_version < CIPHERTEXT_MESSAGE_CURRENT_VERSION {
            return Err(SignalProtocolError::LegacyCiphertextVersion(
                message_version,
            ));
        }
        if message_version > CIPHERTEXT_MESSAGE_CURRENT_VERSION {
            return Err(SignalProtocolError::UnrecognizedCiphertextVersion(
                message_version,
            ));
        }

        let proto_structure =
            proto::wire::SignalMessage::decode(&value[1..value.len() - SignalMessage::MAC_LENGTH])?;

        let sender_ratchet_key = proto_structure
            .ratchet_key
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?;
        let sender_ratchet_key = PublicKey::deserialize(&sender_ratchet_key)?;
        let counter = proto_structure
            .counter
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?;
        let previous_counter = proto_structure.previous_counter.unwrap_or(0);
        let ciphertext = proto_structure
            .ciphertext
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?
            .into_boxed_slice();

        Ok(SignalMessage {
            message_version,
            sender_ratchet_key,
            counter,
            previous_counter,
            ciphertext,
            serialized: Box::from(value),
        })
    }
}

#[derive(Debug, Clone)]
pub struct PreKeySignalMessage {
    message_version: u8,
    registration_id: u32,
    pre_key_id: Option<u32>,
    signed_pre_key_id: u32,
    base_key: PublicKey,
    identity_key: IdentityKey,
    message: SignalMessage,
    serialized: Box<[u8]>,
}

impl PreKeySignalMessage {
    pub fn new(
        message_version: u8,
        registration_id: u32,
        pre_key_id: Option<u32>,
        signed_pre_key_id: u32,
        base_key: PublicKey,
        identity_key: IdentityKey,
        message: SignalMessage,
    ) -> Result<Self> {
        let proto_message = proto::wire::PreKeySignalMessage {
            registration_id: Some(registration_id),
            pre_key_id,
            signed_pre_key_id: Some(signed_pre_key_id),
            base_key: Some(base_key.serialize().into_vec()),
            identity_key: Some(identity_key.serialize().into_vec()),
            message: Some(Vec::from(message.as_ref())),
        };
        let mut serialized = vec![0u8; 1 + proto_message.encoded_len()];
        serialized[0] = ((message_version & 0xF) << 4) | CIPHERTEXT_MESSAGE_CURRENT_VERSION;
        proto_message.encode(&mut &mut serialized[1..])?;
        Ok(Self {
            message_version,
            registration_id,
            pre_key_id,
            signed_pre_key_id,
            base_key,
            identity_key,
            message,
            serialized: serialized.into_boxed_slice(),
        })
    }

    #[inline]
    pub fn message_version(&self) -> u8 {
        self.message_version
    }

    #[inline]
    pub fn registration_id(&self) -> u32 {
        self.registration_id
    }

    #[inline]
    pub fn pre_key_id(&self) -> Option<u32> {
        self.pre_key_id
    }

    #[inline]
    pub fn signed_pre_key_id(&self) -> u32 {
        self.signed_pre_key_id
    }

    #[inline]
    pub fn base_key(&self) -> &PublicKey {
        &self.base_key
    }

    #[inline]
    pub fn identity_key(&self) -> &IdentityKey {
        &self.identity_key
    }

    #[inline]
    pub fn message(&self) -> &SignalMessage {
        &self.message
    }

    #[inline]
    pub fn serialized(&self) -> &[u8] {
        &*self.serialized
    }
}

impl AsRef<[u8]> for PreKeySignalMessage {
    fn as_ref(&self) -> &[u8] {
        &*self.serialized
    }
}

impl TryFrom<&[u8]> for PreKeySignalMessage {
    type Error = SignalProtocolError;

    fn try_from(value: &[u8]) -> Result<Self> {
        if value.is_empty() {
            return Err(SignalProtocolError::CiphertextMessageTooShort(value.len()));
        }

        let message_version = value[0] >> 4;
        if message_version < CIPHERTEXT_MESSAGE_CURRENT_VERSION {
            return Err(SignalProtocolError::LegacyCiphertextVersion(
                message_version,
            ));
        }
        if message_version > CIPHERTEXT_MESSAGE_CURRENT_VERSION {
            return Err(SignalProtocolError::UnrecognizedCiphertextVersion(
                message_version,
            ));
        }

        let proto_structure = proto::wire::PreKeySignalMessage::decode(&value[1..])?;

        let base_key = proto_structure
            .base_key
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?;
        let identity_key = proto_structure
            .identity_key
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?;
        let message = proto_structure
            .message
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?;
        let signed_pre_key_id = proto_structure
            .signed_pre_key_id
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?;

        let base_key = PublicKey::deserialize(base_key.as_ref())?;

        Ok(PreKeySignalMessage {
            message_version,
            registration_id: proto_structure.registration_id.unwrap_or(0),
            pre_key_id: proto_structure.pre_key_id,
            signed_pre_key_id,
            base_key,
            identity_key: IdentityKey::try_from(identity_key.as_ref())?,
            message: SignalMessage::try_from(message.as_ref())?,
            serialized: Box::from(value),
        })
    }
}

#[derive(Debug, Clone)]
pub struct SenderKeyMessage {
    message_version: u8,
    distribution_id: Uuid,
    chain_id: u32,
    iteration: u32,
    ciphertext: Box<[u8]>,
    serialized: Box<[u8]>,
}

impl SenderKeyMessage {
    const SIGNATURE_LEN: usize = 64;

    pub fn new<R: CryptoRng + Rng>(
        message_version: u8,
        distribution_id: Uuid,
        chain_id: u32,
        iteration: u32,
        ciphertext: Box<[u8]>,
        csprng: &mut R,
        signature_key: &PrivateKey,
    ) -> Result<Self> {

         let allbytes = distribution_id.as_bytes();

         let mut dst = [0u8; 4];

         dst.clone_from_slice(&allbytes[0..4]);

         let proto_message = proto::wire::SenderKeyMessage {

             distribution_uuid: Some(u32::from_be_bytes(dst)),

          /*  distribution_uuid: Some(distribution_id.as_bytes().to_vec()), */
          /*  chain_id: Some(chain_id), */
            iteration: Some(iteration),
            ciphertext: Some(ciphertext.to_vec()),
        };

        let chain_id = 0;

        let proto_message_len = proto_message.encoded_len();
        let mut serialized = vec![0u8; 1 + proto_message_len + Self::SIGNATURE_LEN];
        serialized[0] = ((message_version & 0xF) << 4) | SENDERKEY_MESSAGE_CURRENT_VERSION;
        proto_message.encode(&mut &mut serialized[1..1 + proto_message_len])?;
        let signature =
            signature_key.calculate_signature(&serialized[..1 + proto_message_len], csprng)?;
        serialized[1 + proto_message_len..].copy_from_slice(&signature[..]);
        Ok(Self {
            message_version: SENDERKEY_MESSAGE_CURRENT_VERSION,
            distribution_id,
            chain_id,
            iteration,
            ciphertext,
            serialized: serialized.into_boxed_slice(),
        })
    }

    pub fn verify_signature(&self, signature_key: &PublicKey) -> Result<bool> {
        let valid = signature_key.verify_signature(
            &self.serialized[..self.serialized.len() - Self::SIGNATURE_LEN],
            &self.serialized[self.serialized.len() - Self::SIGNATURE_LEN..],
        )?;

        Ok(valid)
    }

    #[inline]
    pub fn message_version(&self) -> u8 {
        self.message_version
    }

    #[inline]
    pub fn distribution_id(&self) -> Uuid {
        self.distribution_id
    }

    #[inline]
    pub fn chain_id(&self) -> u32 {
        self.chain_id
    }

    #[inline]
    pub fn iteration(&self) -> u32 {
        self.iteration
    }

    #[inline]
    pub fn ciphertext(&self) -> &[u8] {
        &*self.ciphertext
    }

    #[inline]
    pub fn serialized(&self) -> &[u8] {
        &*self.serialized
    }
}

impl AsRef<[u8]> for SenderKeyMessage {
    fn as_ref(&self) -> &[u8] {
        &*self.serialized
    }
}

impl TryFrom<&[u8]> for SenderKeyMessage {
    type Error = SignalProtocolError;

    fn try_from(value: &[u8]) -> Result<Self> {
        if value.len() < 1 + Self::SIGNATURE_LEN {
            return Err(SignalProtocolError::CiphertextMessageTooShort(value.len()));
        }
        let message_version = value[0] >> 4;
        if message_version < SENDERKEY_MESSAGE_CURRENT_VERSION {
            return Err(SignalProtocolError::LegacyCiphertextVersion(
                message_version,
            ));
        }
        if message_version > SENDERKEY_MESSAGE_CURRENT_VERSION {
            return Err(SignalProtocolError::UnrecognizedCiphertextVersion(
                message_version,
            ));
        }
        let proto_structure =
            proto::wire::SenderKeyMessage::decode(&value[1..value.len() - Self::SIGNATURE_LEN])?;


        let distribution_id = proto_structure
            .distribution_uuid
          /*  .and_then(|bytes| Uuid::from_slice(bytes.as_slice()).ok()) */
             .and_then(|intval| addUid(intval).ok())
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?;
       /* let chain_id = proto_structure
            .chain_id
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?; */
        let chain_id = 0;

        let iteration = proto_structure
            .iteration
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?;
        let ciphertext = proto_structure
            .ciphertext
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?
            .into_boxed_slice();

        Ok(SenderKeyMessage {
            message_version,
            distribution_id,
            chain_id,
            iteration,
            ciphertext,
            serialized: Box::from(value),
        })
    }
}

fn addUid(intval: u32) -> Option<Uuid> {
     let i = intval.to_be_bytes();
     let arr:[u8; 16] = [
         i[0], i[1], i[2], i[3],
         i[0], i[1], i[2], i[3],
         i[0], i[1], i[2], i[3],
         i[0], i[1], i[2], i[3]
     ];
     return Some(Uuid::from_bytes(arr));
}

#[derive(Debug, Clone)]
pub struct SenderKeyDistributionMessage {
    message_version: u8,
    distribution_id: Uuid,
    chain_id: u32,
    iteration: u32,
    chain_key: Vec<u8>,
    signing_key: PublicKey,
    serialized: Box<[u8]>,
}

impl SenderKeyDistributionMessage {
    pub fn new(
        message_version: u8,
        distribution_id: Uuid,
        chain_id: u32,
        iteration: u32,
        chain_key: Vec<u8>,
        signing_key: PublicKey,
    ) -> Result<Self> {

            let allbytes = distribution_id.as_bytes();

            let mut dst = [0u8; 4];

            dst.clone_from_slice(&allbytes[0..4]);

        let proto_message = proto::wire::SenderKeyDistributionMessage {
            distribution_uuid: Some(u32::from_be_bytes(dst)),
          /*  chain_id: Some(chain_id), */
            iteration: Some(iteration),
            chain_key: Some(chain_key.clone()),
            signing_key: Some(signing_key.serialize().to_vec()),
        };

        let chain_id = 0;

        let mut serialized = vec![0u8; 1 + proto_message.encoded_len()];
        serialized[0] = ((message_version & 0xF) << 4) | SENDERKEY_MESSAGE_CURRENT_VERSION;
        proto_message.encode(&mut &mut serialized[1..])?;

        Ok(Self {
            message_version,
            distribution_id,
            chain_id,
            iteration,
            chain_key,
            signing_key,
            serialized: serialized.into_boxed_slice(),
        })
    }

    #[inline]
    pub fn message_version(&self) -> u8 {
        self.message_version
    }

    #[inline]
    pub fn distribution_id(&self) -> Result<Uuid> {
        Ok(self.distribution_id)
    }

    #[inline]
    pub fn chain_id(&self) -> Result<u32> {
        Ok(self.chain_id)
    }

    #[inline]
    pub fn iteration(&self) -> Result<u32> {
        Ok(self.iteration)
    }

    #[inline]
    pub fn chain_key(&self) -> Result<&[u8]> {
        Ok(&self.chain_key)
    }

    #[inline]
    pub fn signing_key(&self) -> Result<&PublicKey> {
        Ok(&self.signing_key)
    }

    #[inline]
    pub fn serialized(&self) -> &[u8] {
        &*self.serialized
    }
}

impl AsRef<[u8]> for SenderKeyDistributionMessage {
    fn as_ref(&self) -> &[u8] {
        &*self.serialized
    }
}

impl TryFrom<&[u8]> for SenderKeyDistributionMessage {
    type Error = SignalProtocolError;

    fn try_from(value: &[u8]) -> Result<Self> {
        // The message contains at least a X25519 key and a chain key
        if value.len() < 1 + 32 + 32 {
            return Err(SignalProtocolError::CiphertextMessageTooShort(value.len()));
        }

        let message_version = value[0] >> 4;

        if message_version < SENDERKEY_MESSAGE_CURRENT_VERSION {
            return Err(SignalProtocolError::LegacyCiphertextVersion(
                message_version,
            ));
        }
        if message_version > SENDERKEY_MESSAGE_CURRENT_VERSION {
            return Err(SignalProtocolError::UnrecognizedCiphertextVersion(
                message_version,
            ));
        }

        let proto_structure = proto::wire::SenderKeyDistributionMessage::decode(&value[1..])?;

        let distribution_id = proto_structure
            .distribution_uuid
         /*  .and_then(|intval| Uuid::from_slice(intval.to_be_bytes().as_slice()).ok()) */
            .and_then(|intval| addUid(intval).ok())
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?;
        let chain_id = 0;
        /*let chain_id = proto_structure
            .chain_id
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?; */
        let iteration = proto_structure
            .iteration
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?;
        let chain_key = proto_structure
            .chain_key
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?;
        let signing_key = proto_structure
            .signing_key
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?;

        if chain_key.len() != 32 || signing_key.len() != 33 {
            return Err(SignalProtocolError::InvalidProtobufEncoding);
        }

        let signing_key = PublicKey::deserialize(&signing_key)?;

        Ok(SenderKeyDistributionMessage {
            message_version,
            distribution_id,
            chain_id,
            iteration,
            chain_key,
            signing_key,
            serialized: Box::from(value),
        })
    }
}

#[derive(Debug, Clone)]
pub struct PlaintextContent {
    serialized: Box<[u8]>,
}

impl PlaintextContent {
    /// Identifies a serialized PlaintextContent.
    ///
    /// This ensures someone doesn't try to serialize an arbitrary Content message as
    /// PlaintextContent; only messages that are okay to send as plaintext should be allowed.
    const PLAINTEXT_CONTEXT_IDENTIFIER_BYTE: u8 = 0xC0;

    /// Marks the end of a message and the start of any padding.
    ///
    /// Usually messages are padded to avoid exposing patterns,
    /// but PlaintextContent messages are all fixed-length anyway, so there won't be any padding.
    const PADDING_BOUNDARY_BYTE: u8 = 0x80;

    #[inline]
    pub fn body(&self) -> &[u8] {
        &self.serialized[1..]
    }

    #[inline]
    pub fn serialized(&self) -> &[u8] {
        &self.serialized
    }
}

impl From<DecryptionErrorMessage> for PlaintextContent {
    fn from(message: DecryptionErrorMessage) -> Self {
        let proto_structure = proto::service::Content {
            decryption_error_message: Some(message.serialized().to_vec()),
            ..Default::default()
        };
        let mut serialized = vec![Self::PLAINTEXT_CONTEXT_IDENTIFIER_BYTE];
        proto_structure
            .encode(&mut serialized)
            .expect("can always encode to a Vec");
        serialized.push(Self::PADDING_BOUNDARY_BYTE);
        Self {
            serialized: Box::from(serialized),
        }
    }
}

impl TryFrom<&[u8]> for PlaintextContent {
    type Error = SignalProtocolError;

    fn try_from(value: &[u8]) -> Result<Self> {
        if value.is_empty() {
            return Err(SignalProtocolError::CiphertextMessageTooShort(0));
        }
        if value[0] != Self::PLAINTEXT_CONTEXT_IDENTIFIER_BYTE {
            return Err(SignalProtocolError::UnrecognizedMessageVersion(
                value[0] as u32,
            ));
        }
        Ok(Self {
            serialized: Box::from(value),
        })
    }
}

#[derive(Debug, Clone)]
pub struct DecryptionErrorMessage {
    ratchet_key: Option<PublicKey>,
    timestamp: u64,
    device_id: u32,
    serialized: Box<[u8]>,
}

impl DecryptionErrorMessage {
    pub fn for_original(
        original_bytes: &[u8],
        original_type: CiphertextMessageType,
        original_timestamp: u64,
        original_sender_device_id: u32,
    ) -> Result<Self> {
        let ratchet_key = match original_type {
            CiphertextMessageType::Whisper => {
                Some(*SignalMessage::try_from(original_bytes)?.sender_ratchet_key())
            }
            CiphertextMessageType::PreKey => Some(
                *PreKeySignalMessage::try_from(original_bytes)?
                    .message()
                    .sender_ratchet_key(),
            ),
            CiphertextMessageType::SenderKey => None,
            CiphertextMessageType::Plaintext => {
                return Err(SignalProtocolError::InvalidArgument(
                    "cannot create a DecryptionErrorMessage for plaintext content; it is not encrypted".to_string()
                ));
            }
        };

        let proto_message = proto::service::DecryptionErrorMessage {
            timestamp: Some(original_timestamp),
            ratchet_key: ratchet_key.map(|k| k.serialize().into()),
            device_id: Some(original_sender_device_id),
        };
        let mut serialized = Vec::new();
        proto_message.encode(&mut serialized)?;

        Ok(Self {
            ratchet_key,
            timestamp: original_timestamp,
            device_id: original_sender_device_id,
            serialized: serialized.into_boxed_slice(),
        })
    }

    #[inline]
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    #[inline]
    pub fn ratchet_key(&self) -> Option<&PublicKey> {
        self.ratchet_key.as_ref()
    }

    #[inline]
    pub fn device_id(&self) -> u32 {
        self.device_id
    }

    #[inline]
    pub fn serialized(&self) -> &[u8] {
        &self.serialized
    }
}

impl TryFrom<&[u8]> for DecryptionErrorMessage {
    type Error = SignalProtocolError;

    fn try_from(value: &[u8]) -> Result<Self> {
        let proto_structure = proto::service::DecryptionErrorMessage::decode(value)?;
        let timestamp = proto_structure
            .timestamp
            .ok_or(SignalProtocolError::InvalidProtobufEncoding)?;
        let ratchet_key = proto_structure
            .ratchet_key
            .map(|k| PublicKey::deserialize(&k))
            .transpose()?;
        let device_id = proto_structure.device_id.unwrap_or_default();
        Ok(Self {
            timestamp,
            ratchet_key,
            device_id,
            serialized: Box::from(value),
        })
    }
}

/// For testing
pub fn extract_decryption_error_message_from_serialized_content(
    bytes: &[u8],
) -> Result<DecryptionErrorMessage> {
    if bytes.last() != Some(&PlaintextContent::PADDING_BOUNDARY_BYTE) {
        return Err(SignalProtocolError::InvalidProtobufEncoding);
    }
    let content = proto::service::Content::decode(bytes.split_last().expect("checked above").1)?;
    content
        .decryption_error_message
        .as_deref()
        .ok_or_else(|| {
            SignalProtocolError::InvalidArgument(
                "Content does not contain DecryptionErrorMessage".to_owned(),
            )
        })
        .and_then(DecryptionErrorMessage::try_from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KeyPair;

    use rand::rngs::OsRng;
    use rand::{CryptoRng, Rng};

    fn create_signal_message<T>(csprng: &mut T) -> Result<SignalMessage>
    where
        T: Rng + CryptoRng,
    {
        let mut mac_key = [0u8; 32];
        csprng.fill_bytes(&mut mac_key);
        let mac_key = mac_key;

        let mut ciphertext = [0u8; 20];
        csprng.fill_bytes(&mut ciphertext);
        let ciphertext = ciphertext;

        let sender_ratchet_key_pair = KeyPair::generate(csprng);
        let sender_identity_key_pair = KeyPair::generate(csprng);
        let receiver_identity_key_pair = KeyPair::generate(csprng);

        SignalMessage::new(
            3,
            &mac_key,
            sender_ratchet_key_pair.public_key,
            42,
            41,
            &ciphertext,
            &sender_identity_key_pair.public_key.into(),
            &receiver_identity_key_pair.public_key.into(),
        )
    }

    fn assert_signal_message_equals(m1: &SignalMessage, m2: &SignalMessage) {
        assert_eq!(m1.message_version, m2.message_version);
        assert_eq!(m1.sender_ratchet_key, m2.sender_ratchet_key);
        assert_eq!(m1.counter, m2.counter);
        assert_eq!(m1.previous_counter, m2.previous_counter);
        assert_eq!(m1.ciphertext, m2.ciphertext);
        assert_eq!(m1.serialized, m2.serialized);
    }

    #[test]
    fn test_signal_message_serialize_deserialize() -> Result<()> {
        let mut csprng = OsRng;
        let message = create_signal_message(&mut csprng)?;
        let deser_message =
            SignalMessage::try_from(message.as_ref()).expect("should deserialize without error");
        assert_signal_message_equals(&message, &deser_message);
        Ok(())
    }

    #[test]
    fn test_pre_key_signal_message_serialize_deserialize() -> Result<()> {
        let mut csprng = OsRng;
        let identity_key_pair = KeyPair::generate(&mut csprng);
        let base_key_pair = KeyPair::generate(&mut csprng);
        let message = create_signal_message(&mut csprng)?;
        let pre_key_signal_message = PreKeySignalMessage::new(
            3,
            365,
            None,
            97,
            base_key_pair.public_key,
            identity_key_pair.public_key.into(),
            message,
        )?;
        let deser_pre_key_signal_message =
            PreKeySignalMessage::try_from(pre_key_signal_message.as_ref())
                .expect("should deserialize without error");
        assert_eq!(
            pre_key_signal_message.message_version,
            deser_pre_key_signal_message.message_version
        );
        assert_eq!(
            pre_key_signal_message.registration_id,
            deser_pre_key_signal_message.registration_id
        );
        assert_eq!(
            pre_key_signal_message.pre_key_id,
            deser_pre_key_signal_message.pre_key_id
        );
        assert_eq!(
            pre_key_signal_message.signed_pre_key_id,
            deser_pre_key_signal_message.signed_pre_key_id
        );
        assert_eq!(
            pre_key_signal_message.base_key,
            deser_pre_key_signal_message.base_key
        );
        assert_eq!(
            pre_key_signal_message.identity_key.public_key(),
            deser_pre_key_signal_message.identity_key.public_key()
        );
        assert_signal_message_equals(
            &pre_key_signal_message.message,
            &deser_pre_key_signal_message.message,
        );
        assert_eq!(
            pre_key_signal_message.serialized,
            deser_pre_key_signal_message.serialized
        );
        Ok(())
    }

    #[test]
    fn test_sender_key_message_serialize_deserialize() -> Result<()> {
        let mut csprng = OsRng;
        let signature_key_pair = KeyPair::generate(&mut csprng);
        let sender_key_message = SenderKeyMessage::new(
            SENDERKEY_MESSAGE_CURRENT_VERSION,
            Uuid::from_u128(0xd1d1d1d1_7000_11eb_b32a_33b8a8a487a6),
            42,
            7,
            [1u8, 2, 3].into(),
            &mut csprng,
            &signature_key_pair.private_key,
        )?;
        let deser_sender_key_message = SenderKeyMessage::try_from(sender_key_message.as_ref())
            .expect("should deserialize without error");
        assert_eq!(
            sender_key_message.message_version,
            deser_sender_key_message.message_version
        );
        assert_eq!(
            sender_key_message.chain_id,
            deser_sender_key_message.chain_id
        );
        assert_eq!(
            sender_key_message.iteration,
            deser_sender_key_message.iteration
        );
        assert_eq!(
            sender_key_message.ciphertext,
            deser_sender_key_message.ciphertext
        );
        assert_eq!(
            sender_key_message.serialized,
            deser_sender_key_message.serialized
        );
        Ok(())
    }

    #[test]
    fn test_decryption_error_message() -> Result<()> {
        let mut csprng = OsRng;
        let identity_key_pair = KeyPair::generate(&mut csprng);
        let base_key_pair = KeyPair::generate(&mut csprng);
        let message = create_signal_message(&mut csprng)?;
        let timestamp = 0x2_0000_0001;
        let device_id = 0x8086_2021;

        {
            let error_message = DecryptionErrorMessage::for_original(
                message.serialized(),
                CiphertextMessageType::Whisper,
                timestamp,
                device_id,
            )?;
            let error_message = DecryptionErrorMessage::try_from(error_message.serialized())?;
            assert_eq!(
                error_message.ratchet_key(),
                Some(message.sender_ratchet_key())
            );
            assert_eq!(error_message.timestamp(), timestamp);
            assert_eq!(error_message.device_id(), device_id);
        }

        let pre_key_signal_message = PreKeySignalMessage::new(
            3,
            365,
            None,
            97,
            base_key_pair.public_key,
            identity_key_pair.public_key.into(),
            message,
        )?;

        {
            let error_message = DecryptionErrorMessage::for_original(
                pre_key_signal_message.serialized(),
                CiphertextMessageType::PreKey,
                timestamp,
                device_id,
            )?;
            let error_message = DecryptionErrorMessage::try_from(error_message.serialized())?;
            assert_eq!(
                error_message.ratchet_key(),
                Some(pre_key_signal_message.message().sender_ratchet_key())
            );
            assert_eq!(error_message.timestamp(), timestamp);
            assert_eq!(error_message.device_id(), device_id);
        }

        let sender_key_message = SenderKeyMessage::new(
            3,
            Uuid::nil(),
            1,
            2,
            Box::from(b"test".to_owned()),
            &mut csprng,
            &base_key_pair.private_key,
        )?;

        {
            let error_message = DecryptionErrorMessage::for_original(
                sender_key_message.serialized(),
                CiphertextMessageType::SenderKey,
                timestamp,
                device_id,
            )?;
            let error_message = DecryptionErrorMessage::try_from(error_message.serialized())?;
            assert_eq!(error_message.ratchet_key(), None);
            assert_eq!(error_message.timestamp(), timestamp);
            assert_eq!(error_message.device_id(), device_id);
        }

        Ok(())
    }

    #[test]
    fn test_decryption_error_message_for_plaintext() {
        assert!(matches!(
            DecryptionErrorMessage::for_original(&[], CiphertextMessageType::Plaintext, 5, 7),
            Err(SignalProtocolError::InvalidArgument(_))
        ));
    }
}
