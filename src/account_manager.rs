// Copyright 2020 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

#[allow(unused_imports)]
use crate::{
    account::{
        repost_message, Account, AccountHandle, AccountIdentifier, AccountInitialiser, AccountSynchronizer,
        RepostAction, SyncedAccount, SyncedAccountData,
    },
    address::AddressOutput,
    client::ClientOptions,
    event::{
        emit_balance_change, emit_confirmation_state_change, emit_reattachment_event, emit_transaction_event,
        BalanceEvent, TransactionConfirmationChangeEvent, TransactionEvent, TransactionEventType,
        TransactionReattachmentEvent,
    },
    message::{Message, MessagePayload, MessageType, Transfer},
    signing::SignerType,
    storage::{StorageAdapter, Timestamp},
};

use std::{
    collections::HashMap,
    convert::TryInto,
    fs,
    num::NonZeroU64,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use chrono::prelude::*;
use futures::FutureExt;
use getset::Getters;
use iota::{bee_rest_api::types::dtos::LedgerInclusionStateDto, MessageId, OutputId};
use tokio::{
    sync::{
        broadcast::{channel as broadcast_channel, Receiver as BroadcastReceiver, Sender as BroadcastSender},
        Mutex, RwLock,
    },
    time::interval,
};
use zeroize::Zeroize;

/// The default storage folder.
pub const DEFAULT_STORAGE_FOLDER: &str = "./storage";

const DEFAULT_OUTPUT_CONSOLIDATION_THRESHOLD: usize = 100;

/// The default stronghold storage file name.
#[cfg(feature = "stronghold")]
#[cfg_attr(docsrs, doc(cfg(feature = "stronghold")))]
pub const STRONGHOLD_FILENAME: &str = "wallet.stronghold";

/// The default SQLite storage file name.
pub const SQLITE_FILENAME: &str = "wallet.db";

#[doc(hidden)]
pub type AccountStore = Arc<RwLock<HashMap<String, AccountHandle>>>;

/// The storage used by the manager.
enum ManagerStorage {
    /// Stronghold storage.
    Stronghold,
    /// Sqlite storage.
    Sqlite,
}

fn storage_file_path(storage: &ManagerStorage, storage_path: &PathBuf) -> PathBuf {
    if storage_path.is_file() || storage_path.extension().is_some() {
        storage_path.clone()
    } else {
        match storage {
            ManagerStorage::Stronghold => storage_path.join(STRONGHOLD_FILENAME),
            ManagerStorage::Sqlite => storage_path.join(SQLITE_FILENAME),
        }
    }
}

fn storage_password_to_encryption_key(password: &str) -> [u8; 32] {
    let mut dk = [0; 64];
    // safe to unwrap (rounds > 0)
    crypto::keys::pbkdf::PBKDF2_HMAC_SHA512(password.as_bytes(), b"wallet.rs::storage", 100, &mut dk).unwrap();
    let key: [u8; 32] = dk[0..32][..].try_into().unwrap();
    key
}

/// Account manager builder.
pub struct AccountManagerBuilder {
    storage_path: PathBuf,
    storage: ManagerStorage,
    polling_interval: Duration,
    skip_polling: bool,
    storage_encryption_key: Option<[u8; 32]>,
    account_options: AccountOptions,
}

impl Default for AccountManagerBuilder {
    fn default() -> Self {
        Self {
            storage_path: PathBuf::from(DEFAULT_STORAGE_FOLDER),
            storage: ManagerStorage::Sqlite,
            polling_interval: Duration::from_millis(30_000),
            skip_polling: false,
            storage_encryption_key: None,
            account_options: AccountOptions {
                output_consolidation_threshold: DEFAULT_OUTPUT_CONSOLIDATION_THRESHOLD,
                automatic_output_consolidation: true,
                sync_spent_outputs: false,
                persist_events: false,
            },
        }
    }
}

impl AccountManagerBuilder {
    /// Initialises a new instance of the account manager builder with the default storage adapter.
    pub fn new() -> Self {
        Default::default()
    }

    /// Sets the storage config to be used.
    pub fn with_storage(mut self, storage_path: impl AsRef<Path>, password: Option<&str>) -> crate::Result<Self> {
        self.storage_path = storage_path.as_ref().to_path_buf();
        self.storage_encryption_key = match password {
            Some(p) => Some(storage_password_to_encryption_key(p)),
            None => None,
        };
        Ok(self)
    }

    /// Sets the polling interval.
    pub fn with_polling_interval(mut self, polling_interval: Duration) -> Self {
        self.polling_interval = polling_interval;
        self
    }

    pub(crate) fn skip_polling(mut self) -> Self {
        self.skip_polling = true;
        self
    }

    /// Use stronghold as storage system.
    pub(crate) fn with_stronghold_storage(mut self) -> Self {
        self.storage = ManagerStorage::Stronghold;
        self
    }

    /// Sets the number of outputs an address must have to trigger the automatic consolidation process.
    pub fn with_output_consolidation_threshold(mut self, threshold: usize) -> Self {
        self.account_options.output_consolidation_threshold = threshold;
        self
    }

    /// Disables the automatic output consolidation process.
    pub fn with_automatic_output_consolidation_disabled(mut self) -> Self {
        self.account_options.automatic_output_consolidation = true;
        self
    }

    /// Enables fetching spent output history on sync.
    pub fn with_sync_spent_outputs(mut self) -> Self {
        self.account_options.sync_spent_outputs = true;
        self
    }

    /// Enables event persistence.
    pub fn with_event_persistence(mut self) -> Self {
        self.account_options.persist_events = true;
        self
    }

    /// Builds the manager.
    pub async fn finish(self) -> crate::Result<AccountManager> {
        let (storage, storage_file_path, is_stronghold): (Box<dyn StorageAdapter + Send + Sync>, PathBuf, bool) =
            match self.storage {
                ManagerStorage::Stronghold => {
                    let path = storage_file_path(&ManagerStorage::Stronghold, &self.storage_path);
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(&parent)?;
                    }
                    let storage = crate::storage::stronghold::StrongholdStorageAdapter::new(&path)?;
                    (Box::new(storage) as Box<dyn StorageAdapter + Send + Sync>, path, true)
                }
                ManagerStorage::Sqlite => {
                    let path = storage_file_path(&ManagerStorage::Sqlite, &self.storage_path);
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(&parent)?;
                    }
                    let storage = crate::storage::sqlite::SqliteStorageAdapter::new(&path)?;
                    (Box::new(storage) as Box<dyn StorageAdapter + Send + Sync>, path, false)
                }
            };

        crate::storage::set(&storage_file_path, self.storage_encryption_key, storage).await;

        // is_monitoring is set to false if an mqtt error happens, so we can initialize it with `true`.
        let is_monitoring = Arc::new(AtomicBool::new(true));

        // with the stronghold storage, the accounts are loaded when the password is set
        let (accounts, loaded_accounts) = if is_stronghold {
            (Default::default(), false)
        } else {
            AccountManager::load_accounts(&storage_file_path, self.account_options, is_monitoring.clone())
                .await
                .map(|accounts| (accounts, true))
                .unwrap_or_else(|_| (AccountStore::default(), false))
        };
        let mut instance = AccountManager {
            storage_folder: if self.storage_path.is_file() || self.storage_path.extension().is_some() {
                match self.storage_path.parent() {
                    Some(p) => p.to_path_buf(),
                    None => self.storage_path,
                }
            } else {
                self.storage_path
            },
            loaded_accounts,
            storage_path: storage_file_path,
            accounts,
            stop_polling_sender: None,
            polling_handle: None,
            is_monitoring,
            generated_mnemonic: None,
            account_options: self.account_options,
            sync_accounts_lock: Arc::new(Mutex::new(())),
        };

        if !self.skip_polling {
            instance
                .start_background_sync(
                    self.polling_interval,
                    self.account_options.automatic_output_consolidation,
                )
                .await;
        }

        Ok(instance)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AccountOptions {
    pub(crate) output_consolidation_threshold: usize,
    pub(crate) automatic_output_consolidation: bool,
    pub(crate) sync_spent_outputs: bool,
    pub(crate) persist_events: bool,
}

/// The account manager.
///
/// Used to manage multiple accounts.
#[derive(Getters)]
pub struct AccountManager {
    storage_folder: PathBuf,
    loaded_accounts: bool,
    /// the path to the storage.
    #[getset(get = "pub")]
    storage_path: PathBuf,
    /// Returns a handle to the accounts store.
    #[getset(get = "pub")]
    accounts: AccountStore,
    stop_polling_sender: Option<BroadcastSender<()>>,
    polling_handle: Option<thread::JoinHandle<()>>,
    is_monitoring: Arc<AtomicBool>,
    generated_mnemonic: Option<String>,
    account_options: AccountOptions,
    sync_accounts_lock: Arc<Mutex<()>>,
}

impl Clone for AccountManager {
    /// Note that when cloning an AccountManager, the original reference's Drop will stop the background sync.
    /// When the cloned reference is dropped, the background sync system won't be stopped.
    ///
    /// Additionally, the generated mnemonic isn't cloned for security reasons,
    /// so you should store it before cloning.
    fn clone(&self) -> Self {
        Self {
            storage_folder: self.storage_folder.clone(),
            loaded_accounts: self.loaded_accounts,
            storage_path: self.storage_path.clone(),
            accounts: self.accounts.clone(),
            stop_polling_sender: self.stop_polling_sender.clone(),
            polling_handle: None,
            is_monitoring: self.is_monitoring.clone(),
            generated_mnemonic: None,
            account_options: self.account_options,
            sync_accounts_lock: self.sync_accounts_lock.clone(),
        }
    }
}

impl Drop for AccountManager {
    fn drop(&mut self) {
        self.stop_background_sync();
    }
}

#[cfg(feature = "stronghold")]
fn stronghold_password<P: Into<String>>(password: P) -> Vec<u8> {
    let mut password = password.into();
    let mut dk = [0; 64];
    // safe to unwrap because rounds > 0
    crypto::keys::pbkdf::PBKDF2_HMAC_SHA512(password.as_bytes(), b"wallet.rs", 100, &mut dk).unwrap();
    password.zeroize();
    let password: [u8; 32] = dk[0..32][..].try_into().unwrap();
    password.to_vec()
}

impl AccountManager {
    /// Initialises the account manager builder.
    pub fn builder() -> AccountManagerBuilder {
        AccountManagerBuilder::new()
    }

    async fn load_accounts(
        storage_file_path: &PathBuf,
        account_options: AccountOptions,
        is_monitoring: Arc<AtomicBool>,
    ) -> crate::Result<AccountStore> {
        let parsed_accounts = Arc::new(RwLock::new(HashMap::new()));

        let accounts = crate::storage::get(&storage_file_path)
            .await?
            .lock()
            .await
            .get_accounts()
            .await?;
        for account in accounts {
            parsed_accounts.write().await.insert(
                account.id().clone(),
                AccountHandle::new(account, parsed_accounts.clone(), account_options, is_monitoring.clone()),
            );
        }

        Ok(parsed_accounts)
    }

    /// Deletes the storage.
    pub async fn delete(self) -> Result<(), (crate::Error, Self)> {
        self.delete_internal().await.map_err(|e| (e, self))
    }

    pub(crate) async fn delete_internal(&self) -> crate::Result<()> {
        let storage_id = crate::storage::remove(&self.storage_path).await;

        if self.storage_path.exists() {
            std::fs::remove_file(&self.storage_path)?;
        }

        #[cfg(feature = "stronghold")]
        {
            crate::stronghold::unload_snapshot(&self.storage_path, false).await?;
            let _ = std::fs::remove_file(self.stronghold_snapshot_path_internal(&storage_id).await?);
        }

        Ok(())
    }

    #[cfg(feature = "stronghold")]
    pub(crate) async fn stronghold_snapshot_path(&self) -> crate::Result<PathBuf> {
        let storage_id = crate::storage::get(&self.storage_path).await?.lock().await.id();
        self.stronghold_snapshot_path_internal(storage_id).await
    }

    #[cfg(feature = "stronghold")]
    pub(crate) async fn stronghold_snapshot_path_internal(&self, storage_id: &str) -> crate::Result<PathBuf> {
        let stronghold_snapshot_path = if storage_id == crate::storage::stronghold::STORAGE_ID {
            self.storage_path.clone()
        } else {
            self.storage_folder.join(STRONGHOLD_FILENAME)
        };
        Ok(stronghold_snapshot_path)
    }

    // error out if the storage is encrypted
    fn check_storage_encryption(&self) -> crate::Result<()> {
        if self.loaded_accounts {
            Ok(())
        } else {
            Err(crate::Error::StorageIsEncrypted)
        }
    }

    /// Starts monitoring the accounts with the node's mqtt topics.
    async fn start_monitoring(accounts: AccountStore) {
        for account in accounts.read().await.values() {
            crate::monitor::monitor_account_addresses_balance(account.clone()).await;
        }
    }

    /// Initialises the background polling and MQTT monitoring.
    async fn start_background_sync(&mut self, polling_interval: Duration, automatic_output_consolidation: bool) {
        Self::start_monitoring(self.accounts.clone()).await;
        let (stop_polling_sender, stop_polling_receiver) = broadcast_channel(1);
        self.start_polling(polling_interval, stop_polling_receiver, automatic_output_consolidation);
        self.stop_polling_sender = Some(stop_polling_sender);
    }

    /// Stops the background polling and MQTT monitoring.
    pub fn stop_background_sync(&mut self) {
        if let Some(polling_handle) = self.polling_handle.take() {
            self.stop_polling_sender
                .take()
                .unwrap()
                .send(())
                .expect("failed to stop polling process");
            polling_handle.join().expect("failed to join polling thread");
            let accounts = self.accounts.clone();
            thread::spawn(move || {
                crate::block_on(async move {
                    for account_handle in accounts.read().await.values() {
                        let _ = crate::monitor::unsubscribe(account_handle.clone()).await;
                    }
                });
            })
            .join()
            .expect("failed to stop monitoring and polling systems");
        }
    }

    /// Sets the password for the stored accounts.
    pub async fn set_storage_password<P: AsRef<str>>(&mut self, password: P) -> crate::Result<()> {
        let key = storage_password_to_encryption_key(password.as_ref());
        // safe to unwrap because the storage is always defined at this point
        crate::storage::set_encryption_key(&self.storage_path, key)
            .await
            .unwrap();

        if self.accounts.read().await.is_empty() {
            let accounts =
                Self::load_accounts(&self.storage_path, self.account_options, self.is_monitoring.clone()).await?;
            self.loaded_accounts = true;
            {
                let mut accounts_store = self.accounts.write().await;
                for (id, account) in &*accounts.read().await {
                    accounts_store.insert(id.clone(), account.clone());
                }
            }
            crate::spawn(Self::start_monitoring(self.accounts.clone()));
        } else {
            // save the accounts again to reencrypt with the new key
            for account_handle in self.accounts.read().await.values() {
                account_handle.write().await.save().await?;
            }
        }

        Ok(())
    }

    /// Sets the stronghold password.
    pub async fn set_stronghold_password<P: Into<String>>(&mut self, password: P) -> crate::Result<()> {
        let stronghold_path = if self.storage_path.extension().unwrap_or_default() == "stronghold" {
            self.storage_path.clone()
        } else {
            self.storage_folder.join(STRONGHOLD_FILENAME)
        };
        crate::stronghold::load_snapshot(&stronghold_path, stronghold_password(password)).await?;

        if self.accounts.read().await.is_empty() {
            let accounts =
                Self::load_accounts(&self.storage_path, self.account_options, self.is_monitoring.clone()).await?;
            self.loaded_accounts = true;
            {
                let mut accounts_store = self.accounts.write().await;
                for (id, account) in &*accounts.read().await {
                    accounts_store.insert(id.clone(), account.clone());
                }
            }
            crate::spawn(Self::start_monitoring(self.accounts.clone()));
        }

        Ok(())
    }

    /// Changes the stronghold password.
    #[cfg(feature = "stronghold")]
    #[cfg_attr(docsrs, doc(cfg(feature = "stronghold")))]
    pub async fn change_stronghold_password<C: Into<String>, N: Into<String>>(
        &self,
        current_password: C,
        new_password: N,
    ) -> crate::Result<()> {
        crate::stronghold::change_password(
            &self.stronghold_snapshot_path().await?,
            stronghold_password(current_password),
            stronghold_password(new_password),
        )
        .await
        .map_err(|e| e.into())
    }

    /// Determines whether all accounts has the latest address unused.
    pub async fn is_latest_address_unused(&self) -> crate::Result<bool> {
        self.check_storage_encryption()?;
        for account_handle in self.accounts.read().await.values() {
            if !account_handle.is_latest_address_unused().await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Sets the client options for all accounts.
    pub async fn set_client_options(&self, options: ClientOptions) -> crate::Result<()> {
        for account in self.accounts.read().await.values() {
            account.set_client_options(options.clone()).await?;
        }
        Ok(())
    }

    /// Starts the polling mechanism.
    fn start_polling(
        &mut self,
        polling_interval: Duration,
        mut stop: BroadcastReceiver<()>,
        automatic_output_consolidation: bool,
    ) {
        let storage_file_path = self.storage_path.clone();
        let accounts = self.accounts.clone();
        let is_monitoring = self.is_monitoring.clone();
        let account_options = self.account_options;
        let sync_accounts_lock = self.sync_accounts_lock.clone();

        let handle = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let mut interval = interval(polling_interval);
                let mut synced = false;
                loop {
                    tokio::select! {
                        _ = async {
                            interval.tick().await;

                            let storage_file_path_ = storage_file_path.clone();
                            let account_options = account_options;

                            if !accounts.read().await.is_empty() {
                                let should_sync = !(synced && is_monitoring.load(Ordering::Relaxed));
                                match AssertUnwindSafe(
                                    poll(
                                        sync_accounts_lock.clone(),
                                        accounts.clone(),
                                        storage_file_path_,
                                        account_options,
                                        should_sync,
                                        is_monitoring.clone(),
                                        automatic_output_consolidation)
                                    )
                                    .catch_unwind()
                                    .await {
                                        Ok(_) => {
                                            synced = true;
                                        }
                                        Err(error) => {
                                            // if the error isn't a crate::Error type
                                            if error.downcast_ref::<crate::Error>().is_none() {
                                                let msg = if let Some(message) = error.downcast_ref::<String>() {
                                                    format!("Internal error: {}", message)
                                                } else if let Some(message) = error.downcast_ref::<&str>() {
                                                    format!("Internal error: {}", message)
                                                } else {
                                                    "Internal error".to_string()
                                                };
                                                log::error!("[POLLING] error: {}", msg);
                                                let _error = crate::Error::Panic(msg);
                                                // when the error is dropped, the on_error event will be triggered
                                            }
                                        }
                                    }
                            }
                        } => {}
                        _ = stop.recv() => {
                            break;
                        }
                    }
                }
            });
        });
        self.polling_handle = Some(handle);
    }

    /// Stores a mnemonic for the given signer type.
    /// If the mnemonic is not provided, we'll generate one.
    pub async fn store_mnemonic(&mut self, signer_type: SignerType, mnemonic: Option<String>) -> crate::Result<()> {
        let mnemonic = match mnemonic {
            Some(m) => {
                self.verify_mnemonic(&m)?;
                m
            }
            None => self.generate_mnemonic()?,
        };

        let signer = crate::signing::get_signer(&signer_type).await;
        let mut signer = signer.lock().await;
        signer.store_mnemonic(&self.storage_path, mnemonic).await?;

        if let Some(mut mnemonic) = self.generated_mnemonic.take() {
            mnemonic.zeroize();
        }

        Ok(())
    }

    /// Generates a new mnemonic.
    pub fn generate_mnemonic(&mut self) -> crate::Result<String> {
        let mut entropy = [0u8; 32];
        crypto::utils::rand::fill(&mut entropy).map_err(|e| crate::Error::MnemonicEncode(format!("{:?}", e)))?;
        let mnemonic = crypto::keys::bip39::wordlist::encode(&entropy, &crypto::keys::bip39::wordlist::ENGLISH)
            .map_err(|e| crate::Error::MnemonicEncode(format!("{:?}", e)))?;
        self.generated_mnemonic = Some(mnemonic.clone());
        Ok(mnemonic)
    }

    /// Checks is the mnemonic is valid. If a mnemonic was generated with `generate_mnemonic()`, the mnemonic here
    /// should match the generated.
    pub fn verify_mnemonic<S: AsRef<str>>(&mut self, mnemonic: S) -> crate::Result<()> {
        // first we check if the mnemonic is valid to give meaningful errors
        crypto::keys::bip39::wordlist::verify(mnemonic.as_ref(), &crypto::keys::bip39::wordlist::ENGLISH)
            // TODO: crypto::bip39::wordlist::Error should impl Display
            .map_err(|e| crate::Error::InvalidMnemonic(format!("{:?}", e)))?;

        // then we check if the provided mnemonic matches the mnemonic generated with `generate_mnemonic`
        if let Some(generated_mnemonic) = &self.generated_mnemonic {
            if generated_mnemonic != mnemonic.as_ref() {
                return Err(crate::Error::InvalidMnemonic(
                    "doesn't match the generated mnemonic".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Adds a new account.
    pub fn create_account(&self, client_options: ClientOptions) -> crate::Result<AccountInitialiser> {
        self.check_storage_encryption()?;
        Ok(AccountInitialiser::new(
            client_options,
            self.accounts.clone(),
            self.storage_path.clone(),
            self.account_options,
            self.is_monitoring.clone(),
        ))
    }

    /// Deletes an account.
    pub async fn remove_account<I: Into<AccountIdentifier>>(&self, account_id: I) -> crate::Result<()> {
        self.check_storage_encryption()?;

        let account_id = {
            let account_handle = self.get_account(account_id).await?;
            let account = account_handle.read().await;

            if account.balance().total > 0 {
                return Err(crate::Error::AccountNotEmpty);
            }

            account.id().to_string()
        };

        self.accounts.write().await.remove(&account_id);

        crate::storage::get(&self.storage_path)
            .await?
            .lock()
            .await
            .remove_account(&account_id)
            .await?;

        Ok(())
    }

    /// Syncs all accounts.
    pub fn sync_accounts(&self) -> crate::Result<AccountsSynchronizer> {
        self.check_storage_encryption()?;
        Ok(AccountsSynchronizer::new(
            self.sync_accounts_lock.clone(),
            self.accounts.clone(),
            self.storage_path.clone(),
            self.account_options,
            self.is_monitoring.clone(),
        ))
    }

    /// Transfers an amount from an account to another.
    pub async fn internal_transfer<F: Into<AccountIdentifier>, T: Into<AccountIdentifier>>(
        &self,
        from_account_id: F,
        to_account_id: T,
        amount: NonZeroU64,
    ) -> crate::Result<Message> {
        self.check_storage_encryption()?;

        let to_account_handle = self.get_account(to_account_id).await?;
        let to_address = to_account_handle.read().await.latest_address().address().clone();

        let message = self
            .get_account(from_account_id)
            .await?
            .transfer(Transfer::builder(to_address, amount).finish())
            .await?;

        // store the message on the receive account
        let message_ = message.clone();
        to_account_handle
            .write()
            .await
            .do_mut(|account| {
                account.append_messages(vec![message_]);
                Ok(())
            })
            .await?;

        Ok(message)
    }

    /// Backups the storage to the given destination
    pub async fn backup<P: AsRef<Path>>(&self, destination: P, stronghold_password: String) -> crate::Result<PathBuf> {
        let destination = destination.as_ref().to_path_buf();
        if !(destination.is_dir() || destination.parent().map(|parent| parent.is_dir()).unwrap_or_default()) {
            return Err(crate::Error::InvalidBackupDestination);
        }

        let storage_path = {
            // create a account manager to setup the stronghold storage for the backup
            let mut manager = Self::builder()
                .with_storage(&self.storage_folder.join(STRONGHOLD_FILENAME), None)
                .unwrap() // safe to unwrap - password is None
                .skip_polling()
                .with_stronghold_storage()
                .finish()
                .await?;
            manager.set_stronghold_password(stronghold_password).await?;
            let stronghold_storage = crate::storage::get(&self.storage_folder.join(STRONGHOLD_FILENAME)).await?;
            let mut stronghold_storage = stronghold_storage.lock().await;

            for account_handle in self.accounts.read().await.values() {
                stronghold_storage
                    .save_account(&account_handle.read().await.id(), &*account_handle.read().await)
                    .await?;
            }
            self.storage_folder.join(STRONGHOLD_FILENAME)
        };

        if storage_path.exists() {
            let destination = if let Some(filename) = storage_path.file_name() {
                let destination = if destination.is_dir() {
                    destination.join(backup_filename(filename.to_str().unwrap()))
                } else {
                    destination
                };
                let res = fs::copy(storage_path, &destination);

                let mut stronghold_storage = crate::storage::stronghold::StrongholdStorageAdapter::new(
                    &self.storage_folder.join(STRONGHOLD_FILENAME),
                )
                .unwrap();
                for account_handle in self.accounts.read().await.values() {
                    stronghold_storage.remove(&account_handle.read().await.id()).await?;
                }

                res?;
                destination
            } else {
                return Err(crate::Error::StorageDoesntExist);
            };
            Ok(destination)
        } else {
            Err(crate::Error::StorageDoesntExist)
        }
    }

    /// Import backed up accounts.
    pub async fn import_accounts<S: AsRef<Path>>(
        &mut self,
        source: S,
        stronghold_password: String,
    ) -> crate::Result<()> {
        let source = source.as_ref();
        if source.is_dir() || !source.exists() || source.extension().unwrap_or_default() != "stronghold" {
            return Err(crate::Error::InvalidBackupFile);
        }
        if !self.accounts.read().await.is_empty() {
            return Err(crate::Error::StorageExists);
        }

        let storage_file_path = self.storage_folder.join(SQLITE_FILENAME);

        fs::create_dir_all(&self.storage_folder)?;

        let mut stronghold_manager = Self::builder()
            .with_storage(&source, None)
            .unwrap() // safe to unwrap - password is None
            .skip_polling()
            .with_stronghold_storage()
            .finish()
            .await?;
        stronghold_manager
            .set_stronghold_password(stronghold_password.clone())
            .await?;
        for account_handle in stronghold_manager.accounts.read().await.values() {
            account_handle.write().await.set_storage_path(self.storage_path.clone());
        }
        self.accounts = stronghold_manager.accounts.clone();
        self.set_stronghold_password(stronghold_password.clone()).await?;
        for account in self.accounts.read().await.values() {
            account.write().await.save().await?;
        }
        // wait for stronghold to finish its tasks
        let _ = crate::stronghold::actor_runtime().lock().await;
        fs::copy(source, self.storage_folder.join(STRONGHOLD_FILENAME))?;

        #[cfg(feature = "stronghold")]
        {
            // force stronghold to read the snapshot again, ignoring any previous cached value
            crate::stronghold::unload_snapshot(&self.storage_path, false).await?;
            if let Err(e) = self.set_stronghold_password(stronghold_password).await {
                fs::remove_file(&storage_file_path)?;
                return Err(e);
            }
        }

        Ok(())
    }

    /// Gets the account associated with the given identifier.
    pub async fn get_account<I: Into<AccountIdentifier>>(&self, account_id: I) -> crate::Result<AccountHandle> {
        self.check_storage_encryption()?;
        let account_id = account_id.into();
        let accounts = self.accounts.read().await;

        let account = match account_id {
            AccountIdentifier::Id(id) => accounts.get(&id),
            AccountIdentifier::Index(index) => {
                let mut associated_account = None;
                for account_handle in accounts.values() {
                    let account = account_handle.read().await;
                    if account.index() == &index {
                        // if we already found an account with this index,
                        // we error out since this is an incorrect usage of the API
                        // you can't use the index to get an account if you're using multiple signer types
                        // since there's multiple index sequences in that case
                        if associated_account.is_some() {
                            return Err(crate::Error::CannotUseIndexIdentifier);
                        }
                        associated_account = Some(account_handle);
                    }
                }
                associated_account
            }
            AccountIdentifier::Alias(alias) => {
                let mut associated_account = None;
                for account_handle in accounts.values() {
                    let account = account_handle.read().await;
                    if account.alias() == &alias {
                        associated_account = Some(account_handle);
                        break;
                    }
                }
                associated_account
            }
            AccountIdentifier::Address(address) => {
                let mut associated_account = None;
                for account_handle in accounts.values() {
                    let account = account_handle.read().await;
                    if account.addresses().iter().any(|a| a.address() == &address) {
                        associated_account = Some(account_handle);
                        break;
                    }
                }
                associated_account
            }
        };

        account.cloned().ok_or(crate::Error::RecordNotFound)
    }

    /// Gets all accounts from storage.
    pub async fn get_accounts(&self) -> crate::Result<Vec<AccountHandle>> {
        self.check_storage_encryption()?;
        let mut accounts = Vec::new();
        for account in self.accounts.read().await.values() {
            let index = account.index().await;
            accounts.push((index, account.clone()));
        }
        accounts.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(accounts.into_iter().map(|(_, account)| account).collect())
    }

    /// Reattaches an unconfirmed transaction.
    pub async fn reattach<I: Into<AccountIdentifier>>(
        &self,
        account_id: I,
        message_id: &MessageId,
    ) -> crate::Result<Message> {
        self.get_account(account_id).await?.reattach(message_id).await
    }

    /// Promotes an unconfirmed transaction.
    pub async fn promote<I: Into<AccountIdentifier>>(
        &self,
        account_id: I,
        message_id: &MessageId,
    ) -> crate::Result<Message> {
        self.get_account(account_id).await?.promote(message_id).await
    }

    /// Retries an unconfirmed transaction.
    pub async fn retry<I: Into<AccountIdentifier>>(
        &self,
        account_id: I,
        message_id: &MessageId,
    ) -> crate::Result<Message> {
        self.get_account(account_id).await?.retry(message_id).await
    }
}

macro_rules! event_getters_impl {
    ($event_ty:ty, $get_fn_name: ident, $get_count_fn_name: ident) => {
        impl AccountManager {
            /// Gets the paginated events with an optional timestamp filter.
            pub async fn $get_fn_name<T: Into<Option<Timestamp>>>(
                &self,
                count: usize,
                skip: usize,
                from_timestamp: T,
            ) -> crate::Result<Vec<$event_ty>> {
                crate::storage::get(&self.storage_path)
                    .await?
                    .lock()
                    .await
                    .$get_fn_name(count, skip, from_timestamp)
                    .await
            }

            /// Gets the count of events with an optional timestamp filter.
            pub async fn $get_count_fn_name<T: Into<Option<Timestamp>>>(
                &self,
                from_timestamp: T,
            ) -> crate::Result<usize> {
                let count = crate::storage::get(&self.storage_path)
                    .await?
                    .lock()
                    .await
                    .$get_count_fn_name(from_timestamp)
                    .await?;
                Ok(count)
            }
        }
    };
}

event_getters_impl!(BalanceEvent, get_balance_change_events, get_balance_change_event_count);
event_getters_impl!(
    TransactionConfirmationChangeEvent,
    get_transaction_confirmation_events,
    get_transaction_confirmation_event_count
);
event_getters_impl!(
    TransactionEvent,
    get_new_transaction_events,
    get_new_transaction_event_count
);
event_getters_impl!(
    TransactionReattachmentEvent,
    get_reattachment_events,
    get_reattachment_event_count
);
event_getters_impl!(TransactionEvent, get_broadcast_events, get_broadcast_event_count);

/// The accounts synchronizer.
pub struct AccountsSynchronizer {
    mutex: Arc<Mutex<()>>,
    accounts: AccountStore,
    storage_file_path: PathBuf,
    address_index: Option<usize>,
    gap_limit: Option<usize>,
    account_options: AccountOptions,
    is_monitoring: Arc<AtomicBool>,
}

impl AccountsSynchronizer {
    fn new(
        mutex: Arc<Mutex<()>>,
        accounts: AccountStore,
        storage_file_path: PathBuf,
        account_options: AccountOptions,
        is_monitoring: Arc<AtomicBool>,
    ) -> Self {
        Self {
            mutex,
            accounts,
            storage_file_path,
            address_index: None,
            gap_limit: None,
            account_options,
            is_monitoring,
        }
    }

    /// Number of address indexes that are generated.
    pub fn gap_limit(mut self, limit: usize) -> Self {
        self.gap_limit.replace(limit);
        self
    }

    /// Initial address index to start syncing.
    pub fn address_index(mut self, address_index: usize) -> Self {
        self.address_index.replace(address_index);
        self
    }

    /// Syncs the accounts with the Tangle.
    pub async fn execute(self) -> crate::Result<Vec<SyncedAccount>> {
        let accounts = self.accounts.clone();
        for account_handle in accounts.read().await.values() {
            account_handle.disable_mqtt();
        }
        let result = self.execute_internal().await;
        for account_handle in accounts.read().await.values() {
            account_handle.enable_mqtt();
        }
        result
    }

    async fn execute_internal(self) -> crate::Result<Vec<SyncedAccount>> {
        let _lock = self.mutex.lock().await;

        let mut tasks = Vec::new();
        {
            let accounts = self.accounts.read().await;
            let address_index = self.address_index;
            let gap_limit = self.gap_limit;
            for account_handle in accounts.values() {
                let account_handle = account_handle.clone();
                tasks.push(async move {
                    tokio::spawn(async move {
                        let mut sync = account_handle.sync().await;
                        if let Some(index) = address_index {
                            sync = sync.address_index(index);
                        }
                        if let Some(limit) = gap_limit {
                            sync = sync.gap_limit(limit);
                        }
                        let synced_data = sync.get_new_history().await?;
                        crate::Result::Ok((account_handle, synced_data))
                    })
                    .await
                });
            }
        }

        let mut synced_data = Vec::new();
        for res in futures::future::try_join_all(tasks)
            .await
            .expect("failed to sync accounts")
        {
            let (account_handle, data) = res?;
            let account_handle_ = account_handle.clone();
            let mut account = account_handle_.write().await;
            let addresses_before_sync: Vec<(String, u64, HashMap<OutputId, AddressOutput>)> = account
                .addresses()
                .iter()
                .map(|a| (a.address().to_bech32(), *a.balance(), a.outputs().clone()))
                .collect();
            account.append_addresses(data.addresses.to_vec());
            synced_data.push((account_handle, addresses_before_sync, data));
        }

        let mut synced_accounts = Vec::new();
        let mut last_account = None;
        let mut last_account_index = 0;
        for (account_handle, _, _) in &synced_data {
            let account = account_handle.read().await;
            if *account.index() >= last_account_index {
                last_account_index = *account.index();
                last_account = Some((
                    account
                        .addresses()
                        .iter()
                        .all(|addr| *addr.balance() == 0 && addr.outputs().is_empty()),
                    account.client_options().clone(),
                    account.signer_type().clone(),
                ));
            }
        }

        let discovered_accounts_res = match last_account {
            Some((is_empty, client_options, signer_type)) => {
                if !is_empty {
                    log::debug!("[SYNC] running account discovery because the latest account is not empty");
                    discover_accounts(
                        self.accounts.clone(),
                        &self.storage_file_path,
                        &client_options,
                        Some(signer_type),
                        self.account_options,
                        self.is_monitoring.clone(),
                    )
                    .await
                } else {
                    log::debug!("[SYNC] skipping account discovery because the latest account is empty");
                    Ok(vec![])
                }
            }
            None => Ok(vec![]),
        };
        let mut discovered_account_ids = Vec::new();
        if let Ok(discovered_accounts) = discovered_accounts_res {
            if !discovered_accounts.is_empty() {
                let mut accounts = self.accounts.write().await;
                for (account_handle, synced_account_data) in discovered_accounts {
                    let account_handle_ = account_handle.clone();
                    let mut account = account_handle_.write().await;
                    account.set_skip_persistence(false);
                    account.set_addresses(synced_account_data.addresses.to_vec());
                    account.save().await?;
                    accounts.insert(account.id().clone(), account_handle.clone());
                    discovered_account_ids.push(account.id().clone());
                    synced_data.push((account_handle, Vec::new(), synced_account_data));
                }
            }
        }

        for (account_handle, addresses_before_sync, data) in synced_data {
            let mut account = account_handle.write().await;
            let messages_before_sync: Vec<(MessageId, Option<bool>)> =
                account.messages().iter().map(|m| (*m.id(), *m.confirmed())).collect();

            let parsed_messages = data.parse_messages(account_handle.accounts.clone(), &account).await?;
            account.append_messages(parsed_messages.to_vec());
            account.set_last_synced_at(Some(chrono::Local::now()));
            account.save().await?;

            let mut new_messages = Vec::new();
            let mut confirmation_changed_messages = Vec::new();
            for message in parsed_messages {
                if !messages_before_sync.iter().any(|(id, _)| id == message.id()) {
                    new_messages.push(message.clone());
                }
                if messages_before_sync
                    .iter()
                    .any(|(id, confirmed)| id == message.id() && confirmed != message.confirmed())
                {
                    confirmation_changed_messages.push(message);
                }
            }
            if !discovered_account_ids.contains(account.id()) {
                let persist_events = account_handle.account_options.persist_events;
                let events = AccountSynchronizer::get_events(
                    account_handle.account_options,
                    &addresses_before_sync,
                    account.addresses(),
                    &new_messages,
                    &confirmation_changed_messages,
                )
                .await?;
                for message in events.new_transaction_events {
                    emit_transaction_event(TransactionEventType::NewTransaction, &account, message, persist_events)
                        .await?;
                }
                for confirmation_change_event in events.confirmation_change_events {
                    emit_confirmation_state_change(
                        &account,
                        confirmation_change_event.message,
                        confirmation_change_event.confirmed,
                        persist_events,
                    )
                    .await?;
                }
                for balance_change_event in events.balance_change_events {
                    emit_balance_change(
                        &account,
                        &balance_change_event.address,
                        balance_change_event.message_id,
                        balance_change_event.balance_change,
                        persist_events,
                    )
                    .await?;
                }
            }

            // drop the account so SyncedAccount::from doesn't deadlock
            drop(account);
            let mut synced_account = SyncedAccount::from(account_handle.clone()).await;
            let mut updated_messages = new_messages;
            updated_messages.extend(confirmation_changed_messages);
            synced_account.messages = updated_messages;

            let account = account_handle.read().await;
            synced_account.addresses = account
                .addresses()
                .iter()
                .filter(|a| {
                    match addresses_before_sync
                        .iter()
                        .find(|(addr, _, _)| addr == &a.address().to_bech32())
                    {
                        Some((_, balance, outputs)) => balance != a.balance() || outputs != a.outputs(),
                        None => true,
                    }
                })
                .cloned()
                .collect();
            synced_accounts.push(synced_account);
        }

        Ok(synced_accounts)
    }
}

async fn poll(
    sync_accounts_lock: Arc<Mutex<()>>,
    accounts: AccountStore,
    storage_file_path: PathBuf,
    account_options: AccountOptions,
    should_sync: bool,
    is_monitoring: Arc<AtomicBool>,
    automatic_output_consolidation: bool,
) -> crate::Result<()> {
    let retried = if should_sync {
        let synced_accounts = AccountsSynchronizer::new(
            sync_accounts_lock,
            accounts.clone(),
            storage_file_path,
            account_options,
            is_monitoring,
        )
        .execute()
        .await?;

        log::debug!("[POLLING] synced accounts");

        let retried_messages = retry_unconfirmed_transactions(&synced_accounts).await?;
        consolidate_outputs_if_needed(automatic_output_consolidation, &synced_accounts).await?;
        retried_messages
    } else {
        log::info!("[POLLING] skipping syncing process because MQTT is running");
        let mut retried_messages = Vec::new();
        let mut synced_accounts = Vec::new();
        for account_handle in accounts.read().await.values() {
            synced_accounts.push(SyncedAccount::from(account_handle.clone()).await);
            let (account_handle, unconfirmed_messages): (AccountHandle, Vec<(MessageId, Option<MessagePayload>)>) = {
                let unconfirmed_messages = account_handle
                    .read()
                    .await
                    .list_messages(0, 0, Some(MessageType::Unconfirmed))
                    .iter()
                    .map(|m| (*m.id(), m.payload().clone()))
                    .collect();
                (account_handle.clone(), unconfirmed_messages)
            };

            let mut reattachments = Vec::new();
            let mut promotions = Vec::new();
            let mut no_need_promote_or_reattach = Vec::new();
            for (message_id, payload) in unconfirmed_messages {
                match repost_message(account_handle.clone(), &message_id, RepostAction::Retry).await {
                    Ok(new_message) => {
                        if new_message.payload() == &payload {
                            reattachments.push((message_id, new_message));
                        } else {
                            log::info!("[POLLING] promoted and new message is {:?}", new_message.id());
                            promotions.push(new_message);
                        }
                    }
                    Err(crate::Error::ClientError(ref e)) => {
                        if let iota::client::Error::NoNeedPromoteOrReattach(_) = e.as_ref() {
                            no_need_promote_or_reattach.push(message_id);
                        }
                    }
                    _ => {}
                }
            }

            retried_messages.push(RetriedData {
                promoted: promotions,
                reattached: reattachments,
                no_need_promote_or_reattach,
                account_handle,
            });
        }

        consolidate_outputs_if_needed(automatic_output_consolidation, &synced_accounts).await?;

        retried_messages
    };

    for retried_data in retried {
        let mut account = retried_data.account_handle.write().await;
        let client = crate::client::get_client(
            account.client_options(),
            Some(retried_data.account_handle.is_monitoring.clone()),
        )
        .await?;

        for (reattached_message_id, message) in &retried_data.reattached {
            emit_reattachment_event(
                &account,
                *reattached_message_id,
                &message,
                retried_data.account_handle.account_options.persist_events,
            )
            .await?;
        }

        account.append_messages(
            retried_data
                .reattached
                .into_iter()
                .map(|(_, message)| message)
                .collect(),
        );
        account.append_messages(retried_data.promoted);

        for message_id in retried_data.no_need_promote_or_reattach {
            let message = account.get_message_mut(&message_id).unwrap();
            if let Ok(metadata) = client.read().await.get_message().metadata(&message_id).await {
                if let Some(ledger_inclusion_state) = metadata.ledger_inclusion_state {
                    let confirmed = ledger_inclusion_state == LedgerInclusionStateDto::Included;
                    message.set_confirmed(Some(confirmed));
                    let message = message.clone();
                    emit_confirmation_state_change(
                        &account,
                        message,
                        confirmed,
                        retried_data.account_handle.account_options.persist_events,
                    )
                    .await?;
                }
            }
        }
        account.save().await?;
    }
    Ok(())
}

async fn discover_accounts(
    accounts: AccountStore,
    storage_path: &PathBuf,
    client_options: &ClientOptions,
    signer_type: Option<SignerType>,
    account_options: AccountOptions,
    is_monitoring: Arc<AtomicBool>,
) -> crate::Result<Vec<(AccountHandle, SyncedAccountData)>> {
    let mut synced_accounts = vec![];
    let mut index = accounts.read().await.len();
    loop {
        let mut account_initialiser = AccountInitialiser::new(
            client_options.clone(),
            accounts.clone(),
            storage_path.clone(),
            account_options,
            is_monitoring.clone(),
        )
        .skip_persistence()
        .index(index);
        if let Some(signer_type) = &signer_type {
            account_initialiser = account_initialiser.signer_type(signer_type.clone());
        }
        let account_handle = account_initialiser.initialise().await?;
        log::debug!(
            "[SYNC] discovering account {}, signer type {:?}",
            account_handle.read().await.alias(),
            account_handle.read().await.signer_type()
        );
        match account_handle.sync().await.get_new_history().await {
            Ok(synced_account_data) => {
                let is_empty = synced_account_data
                    .addresses
                    .iter()
                    .all(|a| *a.balance() == 0 && a.outputs().is_empty());
                log::debug!("[SYNC] discovered account is empty? {}", is_empty);
                if is_empty {
                    break;
                } else {
                    index += 1;
                    synced_accounts.push((account_handle, synced_account_data));
                }
            }
            Err(e) => {
                log::error!("[SYNC] failed to sync to discover account: {:?}", e);
                // break if the account failed to sync
                // this ensures that the previously discovered accounts get stored.
                break;
            }
        }
    }
    Ok(synced_accounts)
}

struct RetriedData {
    #[allow(dead_code)]
    promoted: Vec<Message>,
    reattached: Vec<(MessageId, Message)>,
    no_need_promote_or_reattach: Vec<MessageId>,
    account_handle: AccountHandle,
}

#[allow(unused_mut)]
async fn consolidate_outputs_if_needed(
    mut automatic_consolidation: bool,
    synced_accounts: &[SyncedAccount],
) -> crate::Result<()> {
    for synced in synced_accounts {
        #[cfg(any(feature = "ledger-nano", feature = "ledger-nano-simulator"))]
        {
            let account = synced.account_handle.read().await;
            let signer_type = account.signer_type();
            if signer_type == &SignerType::LedgerNano || signer_type == &SignerType::LedgerNanoSimulator {
                let addresses = synced.account_handle.output_consolidation_addresses().await;
                for address in addresses {
                    crate::event::emit_address_consolidation_needed(&account, address).await;
                }
                // on ledger we do not consolidate outputs automatically
                automatic_consolidation = false;
            }
        }
        if automatic_consolidation {
            synced.consolidate_outputs().await?;
        }
    }
    Ok(())
}

async fn retry_unconfirmed_transactions(synced_accounts: &[SyncedAccount]) -> crate::Result<Vec<RetriedData>> {
    let mut retried_messages = vec![];
    for synced in synced_accounts {
        let unconfirmed_messages: Vec<(MessageId, Option<MessagePayload>)> = synced
            .account_handle()
            .read()
            .await
            .list_messages(0, 0, Some(MessageType::Unconfirmed))
            .iter()
            .map(|message| (*message.id(), message.payload().clone()))
            .collect();
        let mut reattachments = Vec::new();
        let mut promotions = Vec::new();
        let mut no_need_promote_or_reattach = Vec::new();
        for (message_id, message_payload) in unconfirmed_messages {
            log::debug!("[POLLING] retrying {:?}", message_id);
            match synced.retry(&message_id).await {
                Ok(new_message) => {
                    // if the payload is the same, it was reattached; otherwise it was promoted
                    if new_message.payload() == &message_payload {
                        log::debug!("[POLLING] rettached and new message is {:?}", new_message);
                        reattachments.push((message_id, new_message));
                    } else {
                        log::debug!("[POLLING] promoted and new message is {:?}", new_message);
                        promotions.push(new_message);
                    }
                }
                Err(crate::Error::ClientError(ref e)) => {
                    if let iota::client::Error::NoNeedPromoteOrReattach(_) = e.as_ref() {
                        no_need_promote_or_reattach.push(message_id);
                    }
                }
                _ => {}
            }
        }
        retried_messages.push(RetriedData {
            promoted: promotions,
            reattached: reattachments,
            no_need_promote_or_reattach,
            account_handle: synced.account_handle().clone(),
        });
    }
    Ok(retried_messages)
}

fn backup_filename(original: &str) -> String {
    let date = Local::now();
    format!(
        "{}-iota-wallet-backup{}",
        date.format("%FT%H-%M-%S").to_string(),
        if original.is_empty() {
            "".to_string()
        } else {
            format!("-{}", original)
        }
    )
}

#[cfg(test)]
mod tests {
    use super::ManagerStorage;
    use crate::{
        address::{AddressBuilder, AddressOutput, AddressWrapper, IotaAddress, OutputKind},
        client::ClientOptionsBuilder,
        event::*,
        message::Message,
    };
    use iota::{Ed25519Address, IndexationPayload, MessageBuilder, MessageId, Parents, Payload, TransactionId};
    use std::{collections::HashMap, path::PathBuf};

    #[tokio::test]
    async fn store_accounts() {
        crate::test_utils::with_account_manager(crate::test_utils::TestType::Storage, |manager, _| async move {
            let account_handle = crate::test_utils::AccountCreator::new(&manager).create().await;

            manager
                .remove_account(account_handle.read().await.id())
                .await
                .expect("failed to remove account");
        })
        .await;
    }

    #[tokio::test]
    async fn delete_storage() {
        crate::test_utils::with_account_manager(crate::test_utils::TestType::Storage, |manager, _| async move {
            crate::test_utils::AccountCreator::new(&manager).create().await;
            manager
                .delete()
                .await
                .map_err(|(e, _)| e)
                .expect("failed to delete storage");
        })
        .await;
    }

    #[tokio::test]
    async fn duplicated_alias() {
        let manager = crate::test_utils::get_account_manager().await;

        let client_options = ClientOptionsBuilder::new()
            .with_node("https://api.lb-0.testnet.chrysalis2.com")
            .expect("invalid node URL")
            .build()
            .unwrap();
        let alias = "alias";

        manager
            .create_account(client_options.clone())
            .unwrap()
            .alias(alias)
            .initialise()
            .await
            .expect("failed to add account");

        let second_create_response = manager
            .create_account(client_options)
            .unwrap()
            .alias(alias)
            .initialise()
            .await;
        assert_eq!(second_create_response.is_err(), true);
        match second_create_response.unwrap_err() {
            crate::Error::AccountAliasAlreadyExists => {}
            _ => panic!("unexpected create account response; expected AccountAliasAlreadyExists"),
        }
    }

    #[tokio::test]
    async fn get_account() {
        let manager = crate::test_utils::get_account_manager().await;

        let client_options = ClientOptionsBuilder::new()
            .with_node("https://api.lb-0.testnet.chrysalis2.com")
            .expect("invalid node URL")
            .build()
            .unwrap();

        let account_handle1 = manager
            .create_account(client_options.clone())
            .unwrap()
            .alias("alias")
            .initialise()
            .await
            .expect("failed to add account");
        account_handle1.generate_address().await.unwrap();
        {
            // update address balance so we can create the next account
            let mut account = account_handle1.write().await;
            let mut outputs = HashMap::default();
            let output = AddressOutput {
                transaction_id: TransactionId::new([0; 32]),
                message_id: MessageId::new([0; 32]),
                index: 0,
                amount: 5,
                is_spent: false,
                address: crate::test_utils::generate_random_iota_address(),
                kind: OutputKind::SignatureLockedSingle,
            };
            outputs.insert(output.id().unwrap(), output);
            for address in account.addresses_mut() {
                address.set_outputs(outputs.clone());
            }
        }

        let account_handle2 = manager
            .create_account(client_options)
            .unwrap()
            .alias("alias2")
            .initialise()
            .await
            .expect("failed to add account");
        account_handle2.generate_address().await.unwrap();

        let account1 = &*account_handle1.read().await;
        let account2 = &*account_handle2.read().await;

        assert_eq!(
            account1,
            &*manager.get_account(*account1.index()).await.unwrap().read().await
        );
        assert_eq!(
            account1,
            &*manager.get_account(account1.alias()).await.unwrap().read().await
        );
        assert_eq!(
            account1,
            &*manager.get_account(account1.id()).await.unwrap().read().await
        );
        assert_eq!(
            account1,
            &*manager
                .get_account(account1.addresses().first().unwrap().address().to_bech32())
                .await
                .unwrap()
                .read()
                .await
        );

        assert_eq!(
            account2,
            &*manager.get_account(*account2.index()).await.unwrap().read().await
        );
        assert_eq!(
            account2,
            &*manager.get_account(account2.alias()).await.unwrap().read().await
        );
        assert_eq!(
            account2,
            &*manager.get_account(account2.id()).await.unwrap().read().await
        );
        assert_eq!(
            account2,
            &*manager
                .get_account(account2.addresses().first().unwrap().address().to_bech32())
                .await
                .unwrap()
                .read()
                .await
        );
    }

    #[tokio::test]
    async fn remove_account_with_message_history() {
        crate::test_utils::with_account_manager(crate::test_utils::TestType::Storage, |manager, _| async move {
            let client_options = ClientOptionsBuilder::new()
                .with_node("https://api.lb-0.testnet.chrysalis2.com")
                .expect("invalid node URL")
                .build()
                .unwrap();

            let account_handle = manager
                .create_account(client_options)
                .unwrap()
                .messages(vec![Message::from_iota_message(
                    MessageId::new([0; 32]),
                    MessageBuilder::new()
                        .with_nonce_provider(crate::test_utils::NoopNonceProvider {}, 4000f64, None)
                        .with_parents(Parents::new(vec![MessageId::new([0; 32])]).unwrap())
                        .with_payload(Payload::Indexation(Box::new(
                            IndexationPayload::new(b"index", &[0; 16]).unwrap(),
                        )))
                        .with_network_id(0)
                        .finish()
                        .unwrap(),
                    Default::default(),
                    "",
                    &[crate::test_utils::generate_random_address()],
                    &ClientOptionsBuilder::new().build().unwrap(),
                )
                .with_confirmed(Some(true))
                .finish()
                .await
                .unwrap()])
                .initialise()
                .await
                .unwrap();

            let remove_response = manager.remove_account(account_handle.read().await.id()).await;
            assert!(remove_response.is_ok());
        })
        .await;
    }

    #[tokio::test]
    async fn remove_account_with_balance() {
        crate::test_utils::with_account_manager(crate::test_utils::TestType::Storage, |manager, _| async move {
            let client_options = ClientOptionsBuilder::new()
                .with_node("https://api.lb-0.testnet.chrysalis2.com")
                .expect("invalid node URL")
                .build()
                .unwrap();

            let account_handle = manager
                .create_account(client_options)
                .unwrap()
                .addresses(vec![AddressBuilder::new()
                    .balance(5)
                    .key_index(0)
                    .address(AddressWrapper::new(
                        IotaAddress::Ed25519(Ed25519Address::new([0; 32])),
                        "atoi".to_string(),
                    ))
                    .outputs(vec![AddressOutput {
                        transaction_id: TransactionId::new([0; 32]),
                        message_id: MessageId::new([0; 32]),
                        index: 0,
                        amount: 5,
                        is_spent: false,
                        address: crate::test_utils::generate_random_iota_address(),
                        kind: OutputKind::SignatureLockedSingle,
                    }])
                    .build()
                    .unwrap()])
                .initialise()
                .await
                .unwrap();
            let account = account_handle.read().await;

            let remove_response = manager.remove_account(account.id()).await;
            assert!(remove_response.is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn create_account_with_latest_without_history() {
        crate::test_utils::with_account_manager(crate::test_utils::TestType::Storage, |manager, _| async move {
            let client_options = ClientOptionsBuilder::new()
                .with_node("https://api.lb-0.testnet.chrysalis2.com")
                .expect("invalid node URL")
                .build()
                .unwrap();

            manager
                .create_account(client_options.clone())
                .unwrap()
                .alias("alias")
                .initialise()
                .await
                .expect("failed to add account");

            let create_response = manager.create_account(client_options).unwrap().initialise().await;
            assert!(create_response.is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn create_account_skip_persistence() {
        crate::test_utils::with_account_manager(crate::test_utils::TestType::Storage, |manager, _| async move {
            let client_options = ClientOptionsBuilder::new()
                .with_node("https://api.lb-0.testnet.chrysalis2.com")
                .expect("invalid node URL")
                .with_network("testnet")
                .build()
                .unwrap();

            let account_handle = manager
                .create_account(client_options.clone())
                .unwrap()
                .skip_persistence()
                .initialise()
                .await
                .expect("failed to add account");

            let account_get_res = manager.get_account(account_handle.read().await.id()).await;
            assert!(account_get_res.is_err(), true);
            match account_get_res.unwrap_err() {
                crate::Error::RecordNotFound => {}
                _ => panic!("unexpected get_account response; expected RecordNotFound"),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn backup_and_restore_happy_path() {
        let backup_path = "./backup/happy-path";
        let _ = std::fs::remove_dir_all(backup_path);
        std::fs::create_dir_all(backup_path).unwrap();

        crate::test_utils::with_account_manager(crate::test_utils::TestType::Storage, |manager, _| async move {
            let account_handle = crate::test_utils::AccountCreator::new(&manager).create().await;

            // backup the stored accounts to ./backup/happy-path/${backup_name}
            let backup_path = manager.backup(backup_path, "password".to_string()).await.unwrap();
            let backup_file_path = if backup_path.is_dir() {
                std::fs::read_dir(backup_path)
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path()
                    .to_path_buf()
            } else {
                backup_path
            };
            assert_eq!(backup_file_path.extension().unwrap_or_default(), "stronghold");

            let is_encrypted = crate::storage::get(manager.storage_path())
                .await
                .unwrap()
                .lock()
                .await
                .is_encrypted();

            // get another manager instance so we can import the accounts to a different storage
            #[allow(unused_mut)]
            let mut manager = crate::test_utils::get_account_manager().await;

            #[cfg(feature = "stronghold")]
            {
                // wait for stronghold to finish pending operations and delete the storage file
                crate::stronghold::unload_snapshot(&manager.stronghold_snapshot_path().await.unwrap(), false)
                    .await
                    .unwrap();
                let _ = crate::stronghold::actor_runtime().lock().await;

                if crate::storage::get(manager.storage_path())
                    .await
                    .unwrap()
                    .lock()
                    .await
                    .id()
                    == crate::storage::stronghold::STORAGE_ID
                {
                    let _ = std::fs::remove_file(manager.storage_path());
                }
            }

            if is_encrypted {
                manager.set_storage_password("password").await.unwrap();
            }

            // import the accounts from the backup and assert that it's the same
            manager
                .import_accounts(&backup_file_path, "password".to_string())
                .await
                .unwrap();
            assert!(
                super::storage_file_path(&ManagerStorage::Stronghold, manager.storage_path()).exists(),
                true
            );

            let imported_account = manager.get_account(account_handle.read().await.id()).await.unwrap();
            // set the account storage path field so the assert works
            account_handle
                .write()
                .await
                .set_storage_path(manager.storage_path().clone());
            assert_eq!(&*account_handle.read().await, &*imported_account.read().await);
        })
        .await;
    }

    #[tokio::test]
    async fn backup_and_restore_storage_already_exists() {
        crate::test_utils::with_account_manager(crate::test_utils::TestType::Storage, |mut manager, _| async move {
            let backup_path = PathBuf::from("./backup/account-exists");
            let _ = std::fs::remove_dir_all(&backup_path);
            std::fs::create_dir_all(&backup_path).unwrap();
            // first we'll create an example account
            let address = crate::test_utils::generate_random_iota_address();
            let address = AddressBuilder::new()
                .address(address.clone())
                .key_index(0)
                .balance(0)
                .outputs(vec![])
                .build()
                .unwrap();
            crate::test_utils::AccountCreator::new(&manager)
                .addresses(vec![address])
                .create()
                .await;

            let backup_file_path = backup_path.join("wallet.stronghold");
            let backup_path = manager.backup(&backup_file_path, "password".to_string()).await.unwrap();
            assert_eq!(backup_path, backup_file_path);

            let response = manager.import_accounts(&backup_file_path, "password".to_string()).await;

            assert!(response.is_err());
            assert!(matches!(response.unwrap_err(), crate::Error::StorageExists));
        })
        .await;
    }

    #[tokio::test]
    async fn storage_password_reencrypt() {
        crate::test_utils::with_account_manager(crate::test_utils::TestType::Storage, |mut manager, _| async move {
            crate::test_utils::AccountCreator::new(&manager).create().await;
            manager.set_storage_password("new-password").await.unwrap();
            let account_store = super::AccountManager::load_accounts(
                manager.storage_path(),
                manager.account_options,
                manager.is_monitoring.clone(),
            )
            .await
            .unwrap();
            assert_eq!(account_store.read().await.len(), 1);
        })
        .await;
    }

    #[tokio::test]
    async fn get_balance_change_events() {
        crate::test_utils::with_account_manager(crate::test_utils::TestType::Storage, |manager, _| async move {
            let account_handle = crate::test_utils::AccountCreator::new(&manager).create().await;
            let account = account_handle.read().await;
            let change_events = vec![
                BalanceChange::spent(0),
                BalanceChange::spent(1),
                BalanceChange::spent(2),
                BalanceChange::spent(3),
            ];
            for change in &change_events {
                emit_balance_change(&account, account.latest_address().address(), None, *change, true)
                    .await
                    .unwrap();
            }
            assert!(
                manager.get_balance_change_event_count(None).await.unwrap() == change_events.len(),
                true
            );
            for (take, skip) in &[(2, 0), (2, 2)] {
                let found = manager
                    .get_balance_change_events(*take, *skip, None)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|e| e.balance_change)
                    .collect::<Vec<BalanceChange>>();
                let expected = change_events
                    .clone()
                    .into_iter()
                    .skip(*skip)
                    .take(*take)
                    .collect::<Vec<BalanceChange>>();
                assert!(found == expected, true);
            }
        })
        .await;
    }

    #[tokio::test]
    async fn get_transaction_confirmation_events() {
        crate::test_utils::with_account_manager(crate::test_utils::TestType::Storage, |manager, _| async move {
            let account_handle = crate::test_utils::AccountCreator::new(&manager).create().await;
            let account = account_handle.read().await;
            let m1 = crate::test_utils::GenerateMessageBuilder::default().build().await;
            let m2 = crate::test_utils::GenerateMessageBuilder::default().build().await;
            let m3 = crate::test_utils::GenerateMessageBuilder::default().build().await;
            let confirmation_change_events = vec![
                (m1, true),
                (
                    crate::test_utils::GenerateMessageBuilder::default().build().await,
                    false,
                ),
                (m2, false),
                (m3, true),
            ];
            for (message, change) in &confirmation_change_events {
                emit_confirmation_state_change(&account, message.clone(), *change, true)
                    .await
                    .unwrap();
            }
            assert!(
                manager.get_transaction_confirmation_event_count(None).await.unwrap()
                    == confirmation_change_events.len(),
                true
            );
            for (take, skip) in &[(2, 0), (2, 2)] {
                let found = manager
                    .get_transaction_confirmation_events(*take, *skip, None)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|e| (e.message, e.confirmed))
                    .collect::<Vec<(Message, bool)>>();
                let expected = confirmation_change_events
                    .clone()
                    .into_iter()
                    .skip(*skip)
                    .take(*take)
                    .collect::<Vec<(Message, bool)>>();
                assert!(found == expected, true);
            }
        })
        .await;
    }

    #[tokio::test]
    async fn get_reattachment_events() {
        crate::test_utils::with_account_manager(crate::test_utils::TestType::Storage, |manager, _| async move {
            let account_handle = crate::test_utils::AccountCreator::new(&manager).create().await;
            let account = account_handle.read().await;
            let m1 = crate::test_utils::GenerateMessageBuilder::default().build().await;
            let m2 = crate::test_utils::GenerateMessageBuilder::default().build().await;
            let m3 = crate::test_utils::GenerateMessageBuilder::default().build().await;
            let reattachment_events = vec![
                m1,
                crate::test_utils::GenerateMessageBuilder::default().build().await,
                m2,
                m3,
            ];
            for message in &reattachment_events {
                emit_reattachment_event(&account, *message.id(), message, true)
                    .await
                    .unwrap();
            }
            assert!(
                manager.get_reattachment_event_count(None).await.unwrap() == reattachment_events.len(),
                true
            );
            for (take, skip) in &[(2, 0), (2, 2)] {
                let found = manager
                    .get_reattachment_events(*take, *skip, None)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|e| e.message)
                    .collect::<Vec<Message>>();
                let expected = reattachment_events
                    .clone()
                    .into_iter()
                    .skip(*skip)
                    .take(*take)
                    .collect::<Vec<Message>>();
                assert!(found == expected, true);
            }
        })
        .await;
    }

    macro_rules! transaction_event_test {
        ($event_type: expr, $count_get_fn: ident, $get_fn: ident) => {
            #[tokio::test]
            async fn $get_fn() {
                crate::test_utils::with_account_manager(
                    crate::test_utils::TestType::Storage,
                    |manager, _| async move {
                        let account_handle = crate::test_utils::AccountCreator::new(&manager).create().await;
                        let account = account_handle.read().await;
                        let m1 = crate::test_utils::GenerateMessageBuilder::default().build().await;
                        let m2 = crate::test_utils::GenerateMessageBuilder::default().build().await;
                        let m3 = crate::test_utils::GenerateMessageBuilder::default().build().await;
                        let m4 = crate::test_utils::GenerateMessageBuilder::default().build().await;
                        let events = vec![m1, m2, m3, m4];
                        for message in &events {
                            emit_transaction_event($event_type, &account, message.clone(), true)
                                .await
                                .unwrap();
                        }
                        assert!(manager.$count_get_fn(None).await.unwrap() == events.len(), true);
                        for (take, skip) in &[(2, 0), (2, 2)] {
                            let found = manager
                                .$get_fn(*take, *skip, None)
                                .await
                                .unwrap()
                                .into_iter()
                                .map(|e| e.message)
                                .collect::<Vec<Message>>();
                            let expected = events
                                .clone()
                                .into_iter()
                                .skip(*skip)
                                .take(*take)
                                .collect::<Vec<Message>>();
                            assert!(found == expected, true);
                        }
                    },
                )
                .await;
            }
        };
    }

    transaction_event_test!(
        TransactionEventType::NewTransaction,
        get_new_transaction_event_count,
        get_new_transaction_events
    );
    transaction_event_test!(
        TransactionEventType::Broadcast,
        get_broadcast_event_count,
        get_broadcast_events
    );
}
