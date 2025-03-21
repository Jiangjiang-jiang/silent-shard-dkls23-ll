// Copyright (c) Silence Laboratories Pte. Ltd. All Rights Reserved.
// This software is licensed under the Silence Laboratories License Agreement.

use std::str::FromStr;

use derivation_path::DerivationPath;
use js_sys::{Array, Error, Uint8Array};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use wasm_bindgen::prelude::*;

use dkls23_ll::dsg;
use dkls23_ll::dsg_ot_variant;

use crate::{
    errors::sign_ot_variant_error,
    keyshare::Keyshare,
    maybe_seeded_rng,
    message::{Message, MessageRouting},
};

#[derive(Serialize, Deserialize)]
enum Round {
    Init,
    WaitMsg1,
    WaitMsg2,
    WaitMsg3,
    Pre(dsg::PreSignature),
    WaitMsg4(dsg::PartialSignature),
    Failed,
    Finished,
}

#[derive(Serialize, Deserialize)]
#[wasm_bindgen]
pub struct SignSessionOTVariant {
    state: dsg_ot_variant::State,
    round: Round,
}

#[wasm_bindgen]
impl SignSessionOTVariant {
    /// Create a new session.
    #[wasm_bindgen(constructor)]
    pub fn new(
        keyshare: Keyshare,
        chain_path: &str,
        seed: Option<Vec<u8>>,
    ) -> Self {
        let mut rng = maybe_seeded_rng(seed);

        let chain_path = DerivationPath::from_str(chain_path)
            .expect_throw("invalid derivation path");

        let state = dsg_ot_variant::State::new(
            &mut rng,
            keyshare.into_inner(),
            &chain_path,
        )
        .expect_throw("sign session init");

        SignSessionOTVariant {
            state,
            round: Round::Init,
        }
    }

    /// Serialize session into array of bytes.
    #[wasm_bindgen(js_name = toBytes)]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buffer = vec![];
        ciborium::into_writer(self, &mut buffer)
            .expect_throw("CBOR encode error");

        buffer
    }

    /// Deserialize session from array of bytes.
    #[wasm_bindgen(js_name = fromBytes)]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        ciborium::from_reader(bytes).expect_throw("CBOR decode error")
    }

    /// Return an error message, if any.
    #[wasm_bindgen(js_name = error)]
    pub fn error(&self) -> Option<Error> {
        match &self.round {
            Round::Failed => Some(Error::new("failed")),
            _ => None,
        }
    }

    /// Create a fist message and change session state from Init to WaitMg1.
    #[wasm_bindgen(js_name = createFirstMessage)]
    pub fn create_first_message(&mut self) -> Result<Message, Error> {
        match self.round {
            Round::Init => {
                self.round = Round::WaitMsg1;
                Ok(Message::new(self.state.generate_msg1()))
            }

            _ => Err(Error::new("invalid state")),
        }
    }

    fn handle<T, U, H>(
        &mut self,
        msgs: Vec<Message>,
        mut h: H,
        next: Round,
    ) -> Result<Vec<Message>, Error>
    where
        T: DeserializeOwned,
        U: Serialize + MessageRouting,
        H: FnMut(
            &mut dsg_ot_variant::State,
            Vec<T>,
        )
            -> Result<Vec<U>, dsg_ot_variant::SignOTVariantError>,
    {
        let msgs: Vec<T> = Message::decode_vector(&msgs);
        match h(&mut self.state, msgs) {
            Ok(msgs) => {
                let out = Message::encode_vector(msgs);
                self.round = next;
                Ok(out)
            }

            Err(err) => {
                self.round = Round::Failed;
                Err(sign_ot_variant_error(err))
            }
        }
    }

    /// Handle a batch of messages.
    /// Decode, process and return an array messages to send to other parties.
    #[wasm_bindgen(js_name = handleMessages)]
    pub fn handle_messages(
        &mut self,
        msgs: Vec<Message>,
        seed: Option<Vec<u8>>,
    ) -> Result<Vec<Message>, Error> {
        let mut rng = maybe_seeded_rng(seed);

        match &self.round {
            Round::WaitMsg1 => self.handle(
                msgs,
                |state, msgs| state.handle_msg1(&mut rng, msgs),
                Round::WaitMsg2,
            ),

            Round::WaitMsg2 => self.handle(
                msgs,
                |state, msgs| state.handle_msg2(&mut rng, msgs),
                Round::WaitMsg3,
            ),

            Round::WaitMsg3 => {
                let msgs = Message::decode_vector(&msgs);
                let pre = self
                    .state
                    .handle_msg3(msgs)
                    .map_err(sign_ot_variant_error)?;

                self.round = Round::Pre(pre);

                Ok(vec![])
            }

            Round::Failed => Err(Error::new("failed")),

            _ => Err(Error::new("invalid session state")),
        }
    }

    /// The session contains a "pre-signature".
    /// Returns a last message.
    #[wasm_bindgen(js_name = lastMessage)]
    pub fn last_message(
        &mut self,
        message_hash: &[u8],
    ) -> Result<Message, Error> {
        if message_hash.len() != 32 {
            return Err(Error::new("invalid message hash"));
        }

        match core::mem::replace(&mut self.round, Round::Finished) {
            Round::Pre(pre) => {
                let hash = message_hash.try_into().unwrap();
                let (partial, msg4) =
                    dsg::create_partial_signature(pre, hash);

                self.round = Round::WaitMsg4(partial);

                Ok(Message::new(msg4))
            }

            prev => {
                self.round = prev;
                Err(Error::new("invalid state"))
            }
        }
    }

    /// Combine last messages and return signature as [R, S].
    /// R, S are 32 byte UintArray.
    ///
    /// This method consumes the session and deallocates all
    /// internal data.
    ///
    #[wasm_bindgen(js_name = combine)]
    pub fn combine_partial_signature(
        self,
        msgs: Vec<Message>,
    ) -> Result<Array, Error> {
        match self.round {
            Round::WaitMsg4(partial) => {
                let msgs = Message::decode_vector(&msgs);
                let sign = dsg_ot_variant::combine_signatures(partial, msgs)
                    .map_err(sign_ot_variant_error)?;

                let (r, s) = sign.split_bytes();

                let a = js_sys::Array::new_with_length(2);

                a.set(0, Uint8Array::from(&r as &[u8]).into());
                a.set(1, Uint8Array::from(&s as &[u8]).into());

                Ok(a)
            }

            _ => Err(Error::new("invalid state")),
        }
    }
}

impl MessageRouting for dsg_ot_variant::SignMsg1 {
    fn src_party_id(&self) -> u8 {
        self.from_id
    }

    fn dst_party_id(&self) -> Option<u8> {
        None
    }
}

impl MessageRouting for dsg_ot_variant::SignMsg2 {
    fn src_party_id(&self) -> u8 {
        self.from_id
    }

    fn dst_party_id(&self) -> Option<u8> {
        Some(self.to_id)
    }
}

impl MessageRouting for dsg_ot_variant::SignMsg3 {
    fn src_party_id(&self) -> u8 {
        self.from_id
    }

    fn dst_party_id(&self) -> Option<u8> {
        Some(self.to_id)
    }
}
