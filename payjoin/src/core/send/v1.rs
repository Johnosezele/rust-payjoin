//! Send BIP 78 Payjoin v1
//!
//! This module contains types and methods used to implement sending via [BIP78
//! Payjoin](https://github.com/bitcoin/bips/blob/master/bip-0078.mediawiki).
//!
//! Usage is pretty simple:
//!
//! 1. Parse BIP21 as [`payjoin::Uri`](crate::Uri)
//! 2. Construct URI request parameters, a finalized “Original PSBT” paying .amount to .address
//! 3. (optional) Spawn a thread or async task that will broadcast the original PSBT fallback after
//!    delay (e.g. 1 minute) unless canceled
//! 4. Construct the [`Sender`] using [`SenderBuilder`] with the PSBT and payjoin uri
//! 5. Send the request and receive a response by following on the extracted V1Context
//! 6. Sign and finalize the Payjoin Proposal PSBT
//! 7. Broadcast the Payjoin Transaction (and cancel the optional fallback broadcast)
//!
//! This crate is runtime-agnostic. Data persistence, chain interactions, and networking may be
//! provided by custom implementations or copy the reference
//! [`payjoin-cli`](https://github.com/payjoin/rust-payjoin/tree/master/payjoin-cli) for bitcoind,
//! [`nolooking`](https://github.com/chaincase-app/nolooking) for LND, or
//! [`bitmask-core`](https://github.com/diba-io/bitmask-core) BDK integration. Bring your own
//! wallet and http client.

use bitcoin::psbt::Psbt;
use bitcoin::FeeRate;
use error::BuildSenderError;
use url::Url;

use super::*;
pub use crate::output_substitution::OutputSubstitution;
use crate::{PjUri, Request};

/// A builder to construct the properties of a `Sender`.
#[derive(Clone)]
pub struct SenderBuilder {
    pub(crate) endpoint: Url,
    pub(crate) output_substitution: OutputSubstitution,
    pub(crate) psbt_ctx_builder: PsbtContextBuilder,
}

impl SenderBuilder {
    /// Prepare the context from which to make Sender requests
    ///
    /// Call [`SenderBuilder::build_recommended()`] or other `build` methods
    /// to create a [`Sender`]
    pub fn new(psbt: Psbt, uri: PjUri) -> Self {
        Self {
            endpoint: uri.extras.endpoint,
            // Adopt the output substitution preference from the URI
            output_substitution: uri.extras.output_substitution,
            psbt_ctx_builder: PsbtContextBuilder::new(
                psbt,
                uri.address.script_pubkey(),
                uri.amount,
            ),
        }
    }

    /// Disable output substitution even if the receiver didn't.
    ///
    /// This forbids receiver switching output or decreasing amount.
    /// It is generally **not** recommended to set this as it may prevent the receiver from
    /// doing advanced operations such as opening LN channels and it also guarantees the
    /// receiver will **not** reward the sender with a discount.
    pub fn always_disable_output_substitution(self) -> Self {
        Self { output_substitution: OutputSubstitution::Disabled, ..self }
    }

    // Calculate the recommended fee contribution for an Original PSBT.
    //
    // BIP 78 recommends contributing `originalPSBTFeeRate * vsize(sender_input_type)`.
    // The minfeerate parameter is set if the contribution is available in change.
    //
    // This method fails if no recommendation can be made or if the PSBT is malformed.
    pub fn build_recommended(self, min_fee_rate: FeeRate) -> Result<Sender, BuildSenderError> {
        Ok(Sender {
            endpoint: self.endpoint,
            psbt_ctx: self
                .psbt_ctx_builder
                .build_recommended(min_fee_rate, self.output_substitution)?,
        })
    }

    /// Offer the receiver contribution to pay for his input.
    ///
    /// These parameters will allow the receiver to take `max_fee_contribution` from given change
    /// output to pay for additional inputs. The recommended fee is `size_of_one_input * fee_rate`.
    ///
    /// `change_index` specifies which output can be used to pay fee. If `None` is provided, then
    /// the output is auto-detected unless the supplied transaction has more than two outputs.
    ///
    /// `clamp_fee_contribution` decreases fee contribution instead of erroring.
    ///
    /// If this option is true and a transaction with change amount lower than fee
    /// contribution is provided then instead of returning error the fee contribution will
    /// be just lowered in the request to match the change amount.
    pub fn build_with_additional_fee(
        self,
        max_fee_contribution: bitcoin::Amount,
        change_index: Option<usize>,
        min_fee_rate: FeeRate,
        clamp_fee_contribution: bool,
    ) -> Result<Sender, BuildSenderError> {
        Ok(Sender {
            endpoint: self.endpoint,
            psbt_ctx: self.psbt_ctx_builder.build_with_additional_fee(
                max_fee_contribution,
                change_index,
                min_fee_rate,
                clamp_fee_contribution,
                self.output_substitution,
            )?,
        })
    }

    /// Perform Payjoin without incentivizing the payee to cooperate.
    ///
    /// While it's generally better to offer some contribution some users may wish not to.
    /// This function disables contribution.
    pub fn build_non_incentivizing(
        self,
        min_fee_rate: FeeRate,
    ) -> Result<Sender, BuildSenderError> {
        Ok(Sender {
            endpoint: self.endpoint,
            psbt_ctx: self
                .psbt_ctx_builder
                .build_non_incentivizing(min_fee_rate, self.output_substitution)?,
        })
    }
}

/// A payjoin V1 sender, allowing the construction of a payjoin V1 request
/// and the resulting `V1Context`
#[derive(Clone, Debug)]
#[cfg_attr(feature = "v2", derive(PartialEq, Eq, serde::Serialize, serde::Deserialize))]
pub struct Sender {
    /// The endpoint in the Payjoin URI
    pub(crate) endpoint: Url,
    /// The original PSBT.
    pub(crate) psbt_ctx: PsbtContext,
}

impl Sender {
    /// Construct serialized V1 Request and Context from a Payjoin Proposal
    pub fn create_v1_post_request(&self) -> (Request, V1Context) {
        super::create_v1_post_request(self.endpoint.clone(), self.psbt_ctx.clone())
    }

    /// The endpoint in the Payjoin URI
    pub fn endpoint(&self) -> &Url { &self.endpoint }
}

#[cfg(test)]
mod test {
    use bitcoin::FeeRate;
    use payjoin_test_utils::{
        BoxError, INVALID_PSBT, MULTIPARTY_ORIGINAL_PSBT_ONE, PARSED_ORIGINAL_PSBT,
        PAYJOIN_PROPOSAL,
    };

    use super::*;
    use crate::error_codes::ErrorCode;
    use crate::send::error::{ResponseError, WellKnownError};
    use crate::send::test::create_psbt_context;
    use crate::{Uri, UriExt};

    const PJ_URI: &str =
        "bitcoin:2N47mmrWXsNBvQR6k78hWJoTji57zXwNcU7?amount=0.02&pjos=0&pj=HTTPS://EXAMPLE.COM/";

    fn pj_uri<'a>() -> PjUri<'a> {
        Uri::try_from(PJ_URI)
            .expect("uri should succeed")
            .assume_checked()
            .check_pj_supported()
            .expect("uri should support payjoin")
    }

    fn create_v1_context() -> super::V1Context {
        let psbt_context = create_psbt_context().expect("failed to create context");
        super::V1Context { psbt_context }
    }

    /// This test adds mutation coverage for build_recommended when the outputs are equal to the
    /// payee scripts forcing build_non_incentivising to run.
    #[test]
    fn test_build_recommended_output_is_payee() -> Result<(), BoxError> {
        let mut psbt = PARSED_ORIGINAL_PSBT.clone();
        psbt.unsigned_tx.output[0] = TxOut {
            value: Amount::from_sat(2000000),
            script_pubkey: ScriptBuf::from_hex("a9141de849f069d274150e3afeae8d72eb5a6b09443087")
                .unwrap(),
        };
        psbt.unsigned_tx.output.push(psbt.unsigned_tx.output[1].clone());
        psbt.outputs.push(psbt.outputs[1].clone());
        let sender = SenderBuilder::new(
            psbt.clone(),
            Uri::try_from("bitcoin:34R9npMiyq6KY81DeMMBTgUoAeueyKeycZ?amount=0.02&pjos=0&pj=HTTPS://EXAMPLE.COM/")
                .map_err(|e| format!("{e}"))?
                .assume_checked()
                .check_pj_supported()
                .map_err(|e| format!("{e}"))?,
        )
        .build_recommended(FeeRate::MIN);
        assert!(sender.is_ok(), "{:#?}", sender.err());
        assert_eq!(
            sender.unwrap().psbt_ctx.fee_contribution.unwrap().max_amount,
            Amount::from_sat(0)
        );

        Ok(())
    }

    /// This test is to make sure that the input_pairs for loop inside of build_recommended
    /// runs at least once.
    /// The first branch adds coverage on the for loop and the second branch ensures that the first
    /// and second input_pair are of different address types.
    #[test]
    fn test_build_recommended_multiple_inputs() -> Result<(), BoxError> {
        let mut psbt = Psbt::from_str(MULTIPARTY_ORIGINAL_PSBT_ONE).unwrap();
        let original_psbt = PARSED_ORIGINAL_PSBT.clone();
        psbt.unsigned_tx.input[2] = original_psbt.unsigned_tx.input[0].clone();
        psbt.inputs[2] = original_psbt.inputs[0].clone();
        let sender = SenderBuilder::new(
            psbt.clone(),
            Uri::try_from("bitcoin:bc1qrmzkzmqcgatutq6nyje8t2qs3mf8t3p0qh3kl2?amount=49.99999890&pjos=0&pj=HTTPS://EXAMPLE.COM/")
                .map_err(|e| format!("{e}"))?
                .assume_checked()
                .check_pj_supported()
                .map_err(|e| format!("{e}"))?,
        )
        .build_recommended(FeeRate::MIN);
        assert!(sender.is_ok(), "{:#?}", sender.err());
        assert_eq!(
            sender.unwrap().psbt_ctx.fee_contribution.unwrap().max_amount,
            Amount::from_sat(0)
        );

        let mut psbt = Psbt::from_str(MULTIPARTY_ORIGINAL_PSBT_ONE).unwrap();
        psbt.unsigned_tx.input.pop();
        psbt.inputs.pop();
        let sender = SenderBuilder::new(
            psbt.clone(),
            Uri::try_from("bitcoin:bc1qrmzkzmqcgatutq6nyje8t2qs3mf8t3p0qh3kl2?amount=49.99999890&pjos=0&pj=HTTPS://EXAMPLE.COM/")
                .map_err(|e| format!("{e}"))?
                .assume_checked()
                .check_pj_supported()
                .map_err(|e| format!("{e}"))?,
        )
        .build_recommended(FeeRate::from_sat_per_vb(170000000).expect("Could not determine feerate"));
        assert!(sender.is_ok(), "{:#?}", sender.err());
        assert_eq!(
            sender.unwrap().psbt_ctx.fee_contribution.unwrap().max_amount,
            Amount::from_sat(9999999822)
        );

        Ok(())
    }

    #[test]
    fn test_build_recommended_max_fee_contribution() {
        let psbt = PARSED_ORIGINAL_PSBT.clone();
        let sender = SenderBuilder::new(psbt.clone(), pj_uri())
            .build_recommended(
                FeeRate::from_sat_per_vb(2000000).expect("Could not determine feerate"),
            )
            .expect("sender should succeed");
        assert_eq!(sender.psbt_ctx.output_substitution, OutputSubstitution::Disabled);
        assert_eq!(&sender.psbt_ctx.payee, &pj_uri().address.script_pubkey());
        let fee_contribution =
            sender.psbt_ctx.fee_contribution.expect("sender should contribute fees");
        assert_eq!(fee_contribution.max_amount, psbt.unsigned_tx.output[0].value);
        assert_eq!(fee_contribution.vout, 0);
        assert_eq!(sender.psbt_ctx.min_fee_rate, FeeRate::from_sat_per_kwu(500000000));
    }

    #[test]
    fn test_build_recommended() {
        let sender = SenderBuilder::new(PARSED_ORIGINAL_PSBT.clone(), pj_uri())
            .build_recommended(FeeRate::BROADCAST_MIN)
            .expect("sender should succeed");
        assert_eq!(sender.psbt_ctx.output_substitution, OutputSubstitution::Disabled);
        assert_eq!(&sender.psbt_ctx.payee, &pj_uri().address.script_pubkey());
        let fee_contribution =
            sender.psbt_ctx.fee_contribution.expect("sender should contribute fees");
        assert_eq!(fee_contribution.max_amount, Amount::from_sat(91));
        assert_eq!(fee_contribution.vout, 0);
        assert_eq!(sender.psbt_ctx.min_fee_rate, FeeRate::from_sat_per_kwu(250));
        // Ensure the receiver's output substitution preference is respected either way
        let mut pj_uri = pj_uri();
        pj_uri.extras.output_substitution = OutputSubstitution::Enabled;
        let sender = SenderBuilder::new(PARSED_ORIGINAL_PSBT.clone(), pj_uri)
            .build_recommended(FeeRate::from_sat_per_vb_unchecked(1))
            .expect("sender should succeed");
        assert_eq!(sender.psbt_ctx.output_substitution, OutputSubstitution::Enabled);
    }

    #[test]
    fn handle_json_errors() {
        let ctx = create_v1_context();
        let known_json_error = serde_json::json!({
            "errorCode": "version-unsupported",
            "message": "This version of payjoin is not supported."
        })
        .to_string();
        match ctx.process_response(known_json_error.as_bytes()) {
            Err(ResponseError::WellKnown(WellKnownError {
                code: ErrorCode::VersionUnsupported,
                ..
            })) => (),
            _ => panic!("Expected WellKnownError"),
        }

        let ctx = create_v1_context();
        let invalid_json_error = serde_json::json!({
            "err": "random",
            "message": "This version of payjoin is not supported."
        })
        .to_string();
        match ctx.process_response(invalid_json_error.as_bytes()) {
            Err(ResponseError::Validation(_)) => (),
            _ => panic!("Expected unrecognized JSON error"),
        }
    }

    #[test]
    fn process_response_valid() {
        let ctx = create_v1_context();
        let response = ctx.process_response(PAYJOIN_PROPOSAL.as_bytes());
        assert!(response.is_ok())
    }

    #[test]
    fn process_response_invalid_psbt() {
        let ctx = create_v1_context();
        let response = ctx.process_response(INVALID_PSBT.as_bytes());
        match response {
            Ok(_) => panic!("Invalid PSBT should have caused an error"),
            Err(error) => match error {
                ResponseError::Validation(e) => {
                    assert_eq!(
                        e.to_string(),
                        ValidationError::from(InternalValidationError::Parse).to_string()
                    );
                }
                _ => panic!("Unexpected error type"),
            },
        }
    }

    #[test]
    fn process_response_invalid_utf8() {
        // A PSBT expects an exact match so padding with null bytes for the from_str method is
        // invalid
        let mut invalid_utf8_padding = PAYJOIN_PROPOSAL.as_bytes().to_vec();
        invalid_utf8_padding
            .extend(std::iter::repeat(0x00).take(MAX_CONTENT_LENGTH - invalid_utf8_padding.len()));

        let ctx = create_v1_context();
        let response = ctx.process_response(&invalid_utf8_padding);
        match response {
            Ok(_) => panic!("Invalid UTF-8 should have caused an error"),
            Err(error) => match error {
                ResponseError::Validation(e) => {
                    assert_eq!(
                        e.to_string(),
                        ValidationError::from(InternalValidationError::Parse).to_string()
                    );
                }
                _ => panic!("Unexpected error type"),
            },
        }
    }

    #[test]
    fn process_response_invalid_buffer_len() {
        let mut data = PAYJOIN_PROPOSAL.as_bytes().to_vec();
        data.extend(std::iter::repeat(0).take(MAX_CONTENT_LENGTH + 1));

        let ctx = create_v1_context();
        let response = ctx.process_response(&data);
        match response {
            Ok(_) => panic!("Invalid buffer length should have caused an error"),
            Err(error) => match error {
                ResponseError::Validation(e) => {
                    assert_eq!(
                        e.to_string(),
                        ValidationError::from(InternalValidationError::ContentTooLarge).to_string()
                    );
                }
                _ => panic!("Unexpected error type"),
            },
        }
    }

    #[test]
    fn test_non_witness_input_weight_const() {
        assert_eq!(NON_WITNESS_INPUT_WEIGHT, bitcoin::Weight::from_wu(160));
    }
}
