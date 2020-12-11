// Built-in deps
use std::fmt;

// External uses
use web3::contract::tokens::Tokenize;
use web3::contract::Options;
use web3::types::{Address, BlockNumber, Bytes, TransactionReceipt};
use web3::types::{H160, H256, U256, U64};
use web3::{Error, Transport, Web3};

// Workspace uses
use crate::eth_client_trait::{EthClientInterface, SignedCallResult};
use web3::transports::Http;
use zksync_eth_signer::{raw_ethereum_tx::RawTransaction, EthereumSigner};

/// Gas limit value to be used in transaction if for some reason
/// gas limit was not set for it.
///
/// This is an emergency value, which will not be used normally.
const FALLBACK_GAS_LIMIT: u64 = 3_000_000;

#[derive(Clone)]
pub struct ETHClient<S: EthereumSigner> {
    eth_signer: S,
    sender_account: Address,
    contract_addr: H160,
    contract: ethabi::Contract,
    chain_id: u8,
    gas_price_factor: f64,
    web3: Web3<Http>,
}

impl<S: EthereumSigner> fmt::Debug for ETHClient<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // We do not want to have a private key in the debug representation.

        f.debug_struct("ETHClient")
            .field("sender_account", &self.sender_account)
            .field("contract_addr", &self.contract_addr)
            .field("chain_id", &self.chain_id)
            .field("gas_price_factor", &self.gas_price_factor)
            .finish()
    }
}

impl<S: EthereumSigner> ETHClient<S> {
    pub fn new(
        transport: Http,
        contract: ethabi::Contract,
        operator_eth_addr: H160,
        eth_signer: S,
        contract_eth_addr: H160,
        chain_id: u8,
        gas_price_factor: f64,
    ) -> Self {
        Self {
            sender_account: operator_eth_addr,
            eth_signer,
            contract_addr: contract_eth_addr,
            chain_id,
            contract,
            gas_price_factor,
            web3: Web3::new(transport),
        }
    }
}

#[async_trait::async_trait]
impl<S: EthereumSigner + Sync + Send> EthClientInterface for ETHClient<S> {
    /// Returns the next *expected* nonce with respect to the transactions
    /// in the mempool.
    ///
    /// Note that this method may be inconsistent if used with a cluster of nodes
    /// (e.g. `infura`), since the consecutive tx send and attempt to get a pending
    /// nonce may be routed to the different nodes in cluster, and the latter node
    /// may not know about the send tx yet. Thus it is not recommended to rely on this
    /// method as on the trusted source of the latest nonce.  
    async fn pending_nonce(&self) -> Result<U256, Error> {
        self.web3
            .eth()
            .transaction_count(self.sender_account, Some(BlockNumber::Pending))
            .await
    }

    /// Returns the account nonce based on the last *mined* block. Not mined transactions
    /// (which are in mempool yet) are not taken into account by this method.
    async fn current_nonce(&self) -> Result<U256, Error> {
        self.web3
            .eth()
            .transaction_count(self.sender_account, Some(BlockNumber::Latest))
            .await
    }

    async fn block_number(&self) -> Result<U64, Error> {
        self.web3.eth().block_number().await
    }

    async fn get_gas_price(&self) -> Result<U256, anyhow::Error> {
        let mut network_gas_price = self.web3.eth().gas_price().await?;
        let percent_gas_price_factor = U256::from((self.gas_price_factor * 100.0).round() as u64);
        network_gas_price = (network_gas_price * percent_gas_price_factor) / U256::from(100);
        Ok(network_gas_price)
    }

    /// Returns the account balance.
    async fn balance(&self) -> Result<U256, Error> {
        self.web3.eth().balance(self.sender_account, None).await
    }

    /// Encodes the transaction data (smart contract method and its input) to the bytes
    /// without creating an actual transaction.
    fn encode_tx_data<P: Tokenize>(&self, func: &str, params: P) -> Vec<u8> {
        let f = self
            .contract
            .function(func)
            .expect("failed to get function parameters");
        f.encode_input(&params.into_tokens())
            .expect("failed to encode parameters")
    }

    /// Signs the transaction given the previously encoded data.
    /// Fills in gas/nonce if not supplied inside options.
    async fn sign_prepared_tx(
        &self,
        data: Vec<u8>,
        options: Options,
    ) -> Result<SignedCallResult, anyhow::Error> {
        self.sign_prepared_tx_for_addr(data, self.contract_addr, options)
            .await
    }

    /// Signs the transaction given the previously encoded data.
    /// Fills in gas/nonce if not supplied inside options.
    async fn sign_prepared_tx_for_addr(
        &self,
        data: Vec<u8>,
        contract_addr: H160,
        options: Options,
    ) -> Result<SignedCallResult, anyhow::Error> {
        // fetch current gas_price
        let gas_price = match options.gas_price {
            Some(gas_price) => gas_price,
            None => self.get_gas_price().await?,
        };

        let nonce = match options.nonce {
            Some(nonce) => nonce,
            None => self.pending_nonce().await?,
        };

        let gas = match options.gas {
            Some(gas) => gas,
            None => {
                // Verbosity level is set to `error`, since we expect all the transactions to have
                // a set limit, but don't want to crush the application if for some reason in some
                // place limit was not set.
                log::error!(
                    "No gas limit was set for transaction, using the default limit: {}",
                    FALLBACK_GAS_LIMIT
                );

                U256::from(FALLBACK_GAS_LIMIT)
            }
        };

        // form and sign tx
        let tx = RawTransaction {
            chain_id: self.chain_id,
            nonce,
            to: Some(contract_addr),
            value: options.value.unwrap_or_default(),
            gas_price,
            gas,
            data,
        };

        let signed_tx = self.eth_signer.sign_transaction(tx).await?;
        let hash = self.web3.web3().sha3(Bytes(signed_tx.clone())).await?;

        Ok(SignedCallResult {
            raw_tx: signed_tx,
            gas_price,
            nonce,
            hash,
        })
    }

    /// Encodes the transaction data and signs the transaction.
    /// Fills in gas/nonce if not supplied inside options.
    async fn sign_call_tx<P: Tokenize + Send>(
        &self,
        func: &str,
        params: P,
        options: Options,
    ) -> Result<SignedCallResult, anyhow::Error> {
        let f = self
            .contract
            .function(func)
            .expect("failed to get function parameters");
        let data = f
            .encode_input(&params.into_tokens())
            .expect("failed to encode parameters");

        self.sign_prepared_tx(data, options).await
    }

    /// Sends the transaction to the Ethereum blockchain.
    /// Transaction is expected to be encoded as the byte sequence.
    async fn send_raw_tx(&self, tx: Vec<u8>) -> Result<H256, anyhow::Error> {
        Ok(self.web3.eth().send_raw_transaction(Bytes(tx)).await?)
    }

    /// Gets the Ethereum transaction receipt.
    async fn tx_receipt(&self, tx_hash: H256) -> Result<Option<TransactionReceipt>, anyhow::Error> {
        Ok(self.web3.eth().transaction_receipt(tx_hash).await?)
    }
}
