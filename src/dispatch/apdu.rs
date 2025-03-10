use apdu_app::Interface;
use ctap_types::{serde::error::Error as SerdeError, Error};
use iso7816::{command::CommandView, Data, Status};

use crate::{Authenticator, TrussedRequirements, UserPresence};

pub enum CtapMappingError {
    InvalidCommand(u8),
    ParsingError(SerdeError),
}

impl From<CtapMappingError> for Error {
    fn from(mapping_error: CtapMappingError) -> Error {
        match mapping_error {
            CtapMappingError::InvalidCommand(_cmd) => Error::InvalidCommand,
            CtapMappingError::ParsingError(cbor_error) => match cbor_error {
                SerdeError::SerdeMissingField => Error::MissingParameter,
                _ => Error::InvalidCbor,
            },
        }
    }
}

impl<UP, T, const R: usize> apdu_app::App<R> for Authenticator<UP, T>
where
    UP: UserPresence,
    T: TrussedRequirements,
{
    fn select(
        &mut self,
        interface: Interface,
        _: CommandView<'_>,
        reply: &mut Data<R>,
    ) -> apdu_app::Result {
        // FIDO-over-CCID does not seem to officially be a thing; we don't support it.
        // If we would, need to review the following cases catering to semi-documented U2F legacy.
        if interface != Interface::Contactless {
            return Err(Status::ConditionsOfUseNotSatisfied);
        }

        reply.extend_from_slice(b"U2F_V2").unwrap();
        Ok(())
    }

    fn deselect(&mut self) {}

    fn call(
        &mut self,
        interface: Interface,
        apdu: CommandView<'_>,
        response: &mut Data<R>,
    ) -> apdu_app::Result {
        // FIDO-over-CCID does not seem to officially be a thing; we don't support it.
        // If we would, need to review the following cases catering to semi-documented U2F legacy.
        if interface != Interface::Contactless {
            return Err(Status::ConditionsOfUseNotSatisfied);
        }

        let instruction: u8 = apdu.instruction().into();

        // Officially, only NFCCTAP_MSG (0x10) should occur, which is our FidoCommand::Cbor:
        // <https://fidoalliance.org/specs/fido-v2.1-ps-20210615/fido-client-to-authenticator-protocol-v2.1-ps-20210615.html#nfc-ctap-msg>
        //
        // However, for U2F legacy support (presumably very widespread), after registration
        // "3. Client sends a command for an operation (register / authenticate)"
        // <https://fidoalliance.org/specs/fido-u2f-v1.2-ps-20170411/fido-u2f-nfc-protocol-v1.2-ps-20170411.html>

        match instruction {
            // U2F instruction codes
            // NB(nickray): I don't think 0x00 is a valid case.
            0x00..=0x02 => super::try_handle_ctap1(self, apdu, response)?, //self.call_authenticator_u2f(apdu, response),

            _ => {
                match ctaphid_app::Command::try_from(instruction) {
                    // 0x10
                    Ok(ctaphid_app::Command::Cbor) => {
                        super::handle_ctap2(self, apdu.data(), response)
                    }
                    Ok(ctaphid_app::Command::Msg) => super::try_handle_ctap1(self, apdu, response)?,
                    Ok(ctaphid_app::Command::Deselect) => apdu_app::App::<R>::deselect(self),
                    _ => {
                        info!("Unsupported ins for fido app {:02x}", instruction);
                        return Err(iso7816::Status::InstructionNotSupportedOrInvalid);
                    }
                }
            }
        };
        Ok(())
    }
}
