// Copyright 2020 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use crate::{
    account::{Account, AccountBalance, AccountIdentifier, SyncedAccount},
    address::Address,
    client::ClientOptions,
    message::{Message as WalletMessage, MessageType as WalletMessageType, TransferBuilder},
    signing::SignerType,
    Error,
};
use chrono::{DateTime, Local};
use serde::{ser::Serializer, Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use std::{num::NonZeroU64, path::PathBuf, time::Duration};

/// An account to create.
#[derive(Clone, Debug, Deserialize)]
pub struct AccountToCreate {
    /// The node options.
    #[serde(rename = "clientOptions")]
    pub client_options: ClientOptions,
    /// The account alias.
    pub alias: Option<String>,
    /// The account createdAt date string.
    #[serde(rename = "createdAt")]
    pub created_at: Option<DateTime<Local>>,
    /// Whether to skip saving the account to storage or not.
    #[serde(rename = "skipPersistence", default)]
    pub skip_persistence: bool,
    /// The account's signer type.
    #[serde(rename = "signerType")]
    pub signer_type: Option<SignerType>,
}

/// Each public account method.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "name", content = "data")]
pub enum AccountMethod {
    /// Generate a new unused address.
    GenerateAddress,
    /// Get a unused address.
    GetUnusedAddress,
    /// List messages.
    ListMessages {
        /// Message type filter.
        #[serde(rename = "messageType")]
        message_type: Option<WalletMessageType>,
        /// Number of messages to get.
        #[serde(default)]
        count: usize,
        /// Number of messages to skip.
        #[serde(default)]
        from: usize,
    },
    /// List addresses.
    ListAddresses,
    /// List spent addresses.
    ListSpentAddresses,
    /// List unspent addresses.
    ListUnspentAddresses,
    /// Get account balance information.
    GetBalance,
    /// Get latest address.
    GetLatestAddress,
    /// Sync the account.
    SyncAccount {
        /// The first address index to sync.
        #[serde(rename = "addressIndex")]
        address_index: Option<usize>,
        /// The gap limit.
        #[serde(rename = "gapLimit")]
        gap_limit: Option<usize>,
        /// Whether to skip writing the account in storage or not (defaults to false).
        #[serde(rename = "skipPersistence")]
        skip_persistence: Option<bool>,
    },
    /// Checks if the account's latest address is unused after syncing with the Tangle.
    IsLatestAddressUnused,
    /// Updates the account alias.
    SetAlias(String),
    /// Updates the account client options.
    SetClientOptions(Box<ClientOptions>),
}

/// The messages that can be sent to the actor.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "cmd", content = "payload")]
pub enum MessageType {
    /// Remove the account related to the specified `account_id`.
    RemoveAccount(AccountIdentifier),
    /// Creates an account.
    CreateAccount(Box<AccountToCreate>),
    /// Read account.
    GetAccount(AccountIdentifier),
    /// Read accounts.
    GetAccounts,
    /// Consume an account method.
    CallAccountMethod {
        /// The account identifier.
        #[serde(rename = "accountId")]
        account_id: AccountIdentifier,
        /// The account method to call.
        method: AccountMethod,
    },
    /// Sync accounts.
    SyncAccounts {
        /// The first address index to sync.
        #[serde(rename = "addressIndex")]
        address_index: Option<usize>,
        /// The gap limit.
        #[serde(rename = "gapLimit")]
        gap_limit: Option<usize>,
    },
    /// Reattach message.
    Reattach {
        /// The account identifier.
        #[serde(rename = "accountId")]
        account_id: AccountIdentifier,
        /// The message to reattach.
        #[serde(rename = "message_id")]
        message_id: String,
    },
    /// Backup storage.
    Backup {
        /// The backup destination.
        destination: PathBuf,
        /// Stronghold file password.
        password: String,
    },
    /// Import accounts from storage.
    RestoreBackup {
        /// The path to the backed up storage.
        #[serde(rename = "backupPath")]
        backup_path: String,
        /// Stronghold file password.
        password: String,
    },
    /// Sets the password used to encrypt/decrypt the storage.
    SetStoragePassword(String),
    /// Set stronghold snapshot password.
    #[cfg(feature = "stronghold")]
    #[cfg_attr(docsrs, doc(cfg(feature = "stronghold")))]
    SetStrongholdPassword(String),
    /// Sets the password clear interval.
    #[cfg(feature = "stronghold")]
    #[cfg_attr(docsrs, doc(cfg(feature = "stronghold")))]
    SetStrongholdPasswordClearInterval(Duration),
    /// Get stronghold status.
    #[cfg(feature = "stronghold")]
    #[cfg_attr(docsrs, doc(cfg(feature = "stronghold")))]
    GetStrongholdStatus,
    /// Lock the stronghold snapshot (clears password and unload snapshot from memory).
    #[cfg(feature = "stronghold")]
    #[cfg_attr(docsrs, doc(cfg(feature = "stronghold")))]
    LockStronghold,
    /// Send funds.
    SendTransfer {
        /// The account identifier.
        #[serde(rename = "accountId")]
        account_id: AccountIdentifier,
        /// The transfer details.
        transfer: Box<TransferBuilder>,
    },
    /// Move funds on stored accounts.
    InternalTransfer {
        /// The source account identifier.
        #[serde(rename = "fromAccountId")]
        from_account_id: AccountIdentifier,
        /// The destination account identifier.
        #[serde(rename = "toAccountId")]
        to_account_id: AccountIdentifier,
        /// The transfer amount.
        amount: NonZeroU64,
    },
    /// Generates a new mnemonic.
    GenerateMnemonic,
    /// Checks if the given mnemonic is valid.
    VerifyMnemonic(String),
    /// Store mnemonic.
    StoreMnemonic {
        /// The signer type.
        #[serde(rename = "signerType")]
        signer_type: SignerType,
        /// The mnemonic. If empty, we'll generate one.
        mnemonic: Option<String>,
    },
    /// Checks if all accounts has unused latest address after syncing with the Tangle.
    IsLatestAddressUnused,
    /// Get the Ledger Nano or Speculos simulator status.
    #[cfg(any(feature = "ledger-nano", feature = "ledger-nano-simulator"))]
    #[cfg_attr(docsrs, doc(cfg(any(feature = "ledger-nano", feature = "ledger-nano-simulator"))))]
    GetLedgerStatus(bool),
    /// Deletes the storage.
    DeleteStorage,
    /// Changes stronghold snapshot password.
    #[cfg(feature = "stronghold")]
    #[cfg_attr(docsrs, doc(cfg(feature = "stronghold")))]
    ChangeStrongholdPassword {
        /// The current stronghold password.
        #[serde(rename = "currentPassword")]
        current_password: String,
        /// The new stronghold password.
        #[serde(rename = "newPassword")]
        new_password: String,
    },
    /// Updates the client options for all accounts.
    SetClientOptions(Box<ClientOptions>),
}

impl Serialize for MessageType {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            MessageType::RemoveAccount(_) => serializer.serialize_unit_variant("MessageType", 0, "RemoveAccount"),
            MessageType::CreateAccount(_) => serializer.serialize_unit_variant("MessageType", 1, "CreateAccount"),
            MessageType::GetAccount(_) => serializer.serialize_unit_variant("MessageType", 2, "GetAccount"),
            MessageType::GetAccounts => serializer.serialize_unit_variant("MessageType", 3, "GetAccounts"),
            MessageType::CallAccountMethod {
                account_id: _,
                method: _,
            } => serializer.serialize_unit_variant("MessageType", 4, "CallAccountMethod"),
            MessageType::SyncAccounts {
                address_index: _,
                gap_limit: _,
            } => serializer.serialize_unit_variant("MessageType", 5, "SyncAccounts"),
            MessageType::Reattach {
                account_id: _,
                message_id: _,
            } => serializer.serialize_unit_variant("MessageType", 6, "Reattach"),
            MessageType::Backup {
                destination: _,
                password: _,
            } => serializer.serialize_unit_variant("MessageType", 7, "Backup"),
            MessageType::RestoreBackup {
                backup_path: _,
                password: _,
            } => serializer.serialize_unit_variant("MessageType", 8, "RestoreBackup"),
            MessageType::SetStoragePassword(_) => {
                serializer.serialize_unit_variant("MessageType", 9, "SetStoragePassword")
            }
            #[cfg(feature = "stronghold")]
            MessageType::SetStrongholdPassword(_) => {
                serializer.serialize_unit_variant("MessageType", 10, "SetStrongholdPassword")
            }
            #[cfg(feature = "stronghold")]
            MessageType::SetStrongholdPasswordClearInterval(_) => {
                serializer.serialize_unit_variant("MessageType", 11, "SetStrongholdPasswordClearInterval")
            }
            #[cfg(feature = "stronghold")]
            MessageType::GetStrongholdStatus => {
                serializer.serialize_unit_variant("MessageType", 12, "GetStrongholdStatus")
            }
            #[cfg(feature = "stronghold")]
            MessageType::LockStronghold => serializer.serialize_unit_variant("MessageType", 13, "LockStronghold"),
            MessageType::SendTransfer {
                account_id: _,
                transfer: _,
            } => serializer.serialize_unit_variant("MessageType", 14, "SendTransfer"),
            MessageType::InternalTransfer {
                from_account_id: _,
                to_account_id: _,
                amount: _,
            } => serializer.serialize_unit_variant("MessageType", 15, "InternalTransfer"),
            MessageType::GenerateMnemonic => serializer.serialize_unit_variant("MessageType", 16, "GenerateMnemonic"),
            MessageType::VerifyMnemonic(_) => serializer.serialize_unit_variant("MessageType", 17, "VerifyMnemonic"),
            MessageType::StoreMnemonic {
                signer_type: _,
                mnemonic: _,
            } => serializer.serialize_unit_variant("MessageType", 18, "StoreMnemonic"),
            MessageType::IsLatestAddressUnused => {
                serializer.serialize_unit_variant("MessageType", 19, "IsLatestAddressUnused")
            }
            #[cfg(any(feature = "ledger-nano", feature = "ledger-nano-simulator"))]
            MessageType::GetLedgerStatus(_) => serializer.serialize_unit_variant("MessageType", 20, "GetLedgerStatus"),
            MessageType::DeleteStorage => serializer.serialize_unit_variant("MessageType", 21, "DeleteStorage"),
            #[cfg(feature = "stronghold")]
            MessageType::ChangeStrongholdPassword {
                current_password: _,
                new_password: _,
            } => serializer.serialize_unit_variant("MessageType", 22, "ChangeStrongholdPassword"),
            MessageType::SetClientOptions(_) => {
                serializer.serialize_unit_variant("MessageType", 23, "SetClientOptions")
            }
        }
    }
}

/// The actor response type.
#[derive(Serialize, Debug)]
pub struct Response {
    id: String,
    #[serde(flatten)]
    response: ResponseType,
    action: MessageType,
}

impl Response {
    /// Creates a new response.
    pub fn new<S: Into<String>>(id: S, action: MessageType, response: ResponseType) -> Self {
        Self {
            id: id.into(),
            response,
            action,
        }
    }

    /// The response's type.
    pub fn response(&self) -> &ResponseType {
        &self.response
    }
}

/// The response message.
#[derive(Serialize, Debug)]
#[serde(tag = "type", content = "payload")]
pub enum ResponseType {
    /// Account succesfully removed.
    RemovedAccount(AccountIdentifier),
    /// Account succesfully created.
    CreatedAccount(Account),
    /// GetAccount response.
    ReadAccount(Account),
    /// GetAccounts response.
    ReadAccounts(Vec<Account>),
    /// ListMessages response.
    Messages(Vec<WalletMessage>),
    /// ListAddresses/ListSpentAddresses/ListUnspentAddresses response.
    Addresses(Vec<Address>),
    /// GenerateAddress response.
    GeneratedAddress(Address),
    /// GetUnusedAddress response.
    UnusedAddress(Address),
    /// GetLatestAddress response.
    LatestAddress(Address),
    /// GetBalance response.
    Balance(AccountBalance),
    /// SyncAccounts response.
    SyncedAccounts(Vec<SyncedAccount>),
    /// SyncAccount response.
    SyncedAccount(SyncedAccount),
    /// Reattach response.
    Reattached(String),
    /// Backup response.
    BackupSuccessful,
    /// ImportAccounts response.
    BackupRestored,
    /// SetStoragePassword response.
    StoragePasswordSet,
    /// SetStrongholdPassword response.
    #[cfg(feature = "stronghold")]
    #[cfg_attr(docsrs, doc(cfg(feature = "stronghold")))]
    StrongholdPasswordSet,
    /// SetStrongholdPasswordClearInterval response.
    #[cfg(feature = "stronghold")]
    #[cfg_attr(docsrs, doc(cfg(feature = "stronghold")))]
    StrongholdPasswordClearIntervalSet,
    /// GetStrongholdStatus response.
    #[cfg(feature = "stronghold")]
    #[cfg_attr(docsrs, doc(cfg(feature = "stronghold")))]
    StrongholdStatus(crate::stronghold::Status),
    /// LockStronghold response.
    #[cfg(feature = "stronghold")]
    #[cfg_attr(docsrs, doc(cfg(feature = "stronghold")))]
    LockedStronghold,
    /// SendTransfer and InternalTransfer response.
    SentTransfer(WalletMessage),
    /// An error occurred.
    Error(Error),
    /// A panic occurred.
    Panic(String),
    /// GenerateMnemonic response.
    GeneratedMnemonic(String),
    /// VerifyMnemonic response.
    VerifiedMnemonic,
    /// StoreMnemonic response.
    StoredMnemonic,
    /// AccountMethod's IsLatestAddressUnused response.
    IsLatestAddressUnused(bool),
    /// IsLatestAddressUnused response.
    AreAllLatestAddressesUnused(bool),
    /// SetAlias response.
    UpdatedAlias,
    /// Account method SetClientOptions response.
    UpdatedClientOptions,
    /// GetLedgerStatus response.
    #[cfg(any(feature = "ledger-nano", feature = "ledger-nano-simulator"))]
    #[cfg_attr(docsrs, doc(cfg(any(feature = "ledger-nano", feature = "ledger-nano-simulator"))))]
    LedgerStatus(crate::LedgerStatus),
    /// DeleteStorage response.
    DeletedStorage,
    /// ChangeStrongholdPassword response.
    #[cfg(feature = "stronghold")]
    #[cfg_attr(docsrs, doc(cfg(feature = "stronghold")))]
    StrongholdPasswordChanged,
    /// SetClientOptions response.
    UpdatedAllClientOptions,
}

/// The message type.
#[derive(Debug, Clone)]
pub struct Message {
    id: String,
    pub(crate) message_type: MessageType,
    pub(crate) response_tx: UnboundedSender<Response>,
}

impl Message {
    /// Creates a new instance of a Message.
    pub fn new<S: Into<String>>(id: S, message_type: MessageType, response_tx: UnboundedSender<Response>) -> Self {
        Self {
            id: id.into(),
            message_type,
            response_tx,
        }
    }

    /// The message type.
    pub fn message_type(&self) -> &MessageType {
        &self.message_type
    }

    /// The message type.
    pub(crate) fn message_type_mut(&mut self) -> &mut MessageType {
        &mut self.message_type
    }

    /// The response sender.
    pub fn response_tx(&self) -> &UnboundedSender<Response> {
        &self.response_tx
    }

    /// The message identifier.
    pub fn id(&self) -> &String {
        &self.id
    }
}
