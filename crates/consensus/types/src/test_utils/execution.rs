// Copyright (c) Telcoin, LLC
// SPDX-License-Identifier: Apache-2.0

//! Specific test utils for execution layer
use crate::{adiri_genesis, Batch, BatchAPI as _, ExecutionKeypair};
use rand::{rngs::StdRng, SeedableRng};
use reth_chainspec::{BaseFeeParams, ChainSpec};
use reth_primitives::{
    public_key_to_address, sign_message, Address, FromRecoveredPooledTransaction, Genesis,
    GenesisAccount, PooledTransactionsElement, Signature, Transaction, TransactionSigned,
    TxEip1559, TxHash, TxKind, B256, U256,
};
use reth_provider::BlockReaderIdExt;
use reth_transaction_pool::{TransactionOrigin, TransactionPool};
use secp256k1::Secp256k1;
use std::{str::FromStr as _, sync::Arc};

/// Adiri genesis with funded [TransactionFactory] default account.
pub fn test_genesis() -> Genesis {
    let genesis = adiri_genesis();
    let default_address = TransactionFactory::default().address();
    let default_factory_account =
        vec![(default_address, GenesisAccount::default().with_balance(U256::MAX))];
    genesis.extend_accounts(default_factory_account)
}

/// Helper function to seed an instance of Genesis with random batches.
///
/// The transactions in the randomly generated batches are decoded and their signers are recovered.
///
/// The function returns the new Genesis, the signed transactions, and the addresses for further use it testing.
pub fn seeded_genesis_from_random_batches<'a>(
    genesis: Genesis,
    batches: impl IntoIterator<Item = &'a Batch>,
) -> (Genesis, Vec<TransactionSigned>, Vec<Address>) {
    let mut txs = vec![];
    let mut senders = vec![];
    let mut accounts_to_seed = Vec::new();
    for batch in batches {
        for tx in batch.transactions_owned() {
            let tx_signed =
                TransactionSigned::decode_enveloped(&mut tx.as_ref()).expect("decode tx signed");
            let address = tx_signed.recover_signer().expect("signer recoverable");
            txs.push(tx_signed);
            senders.push(address);
            // fund account with 99mil TEL
            let account = (
                address,
                GenesisAccount::default().with_balance(
                    U256::from_str("0x51E410C0F93FE543000000").expect("account balance is parsed"),
                ),
            );
            accounts_to_seed.push(account);
        }
    }
    (genesis.extend_accounts(accounts_to_seed), txs, senders)
}

/// Transaction factory
pub struct TransactionFactory {
    /// Keypair for signing transactions
    keypair: ExecutionKeypair,
    /// The nonce for the next transaction constructed.
    nonce: u64,
}

impl Default for TransactionFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl TransactionFactory {
    /// Create a new instance of self from a [0; 32] seed.
    ///
    /// Address: 0xb14d3c4f5fbfbcfb98af2d330000d49c95b93aa7
    /// Secret: 9bf49a6a0755f953811fce125f2683d50429c3bb49e074147e0089a52eae155f
    pub fn new() -> Self {
        let mut rng = StdRng::from_seed([0; 32]);
        let secp = Secp256k1::new();
        let (secret_key, _public_key) = secp.generate_keypair(&mut rng);
        let keypair = ExecutionKeypair::from_secret_key(&secp, &secret_key);
        Self { keypair, nonce: 0 }
    }

    /// Create a new instance of self from a random seed.
    pub fn new_random() -> Self {
        let secp = Secp256k1::new();
        let (secret_key, _public_key) = secp.generate_keypair(&mut rand::thread_rng());
        let keypair = ExecutionKeypair::from_secret_key(&secp, &secret_key);
        Self { keypair, nonce: 0 }
    }

    /// Return the address of the signer.
    pub fn address(&self) -> Address {
        let public_key = self.keypair.public_key();
        public_key_to_address(public_key)
    }

    /// Change the nonce for the next transaction.
    pub fn set_nonce(&mut self, nonce: u64) {
        self.nonce = nonce;
    }

    /// Increment nonce after a transaction was created and signed.
    fn inc_nonce(&mut self) {
        self.nonce += 1;
    }

    /// Create and sign an EIP1559 transaction.
    pub fn create_eip1559(
        &mut self,
        chain: Arc<ChainSpec>,
        gas_price: u128,
        to: Address,
        value: U256,
    ) -> TransactionSigned {
        // Eip1559
        let transaction = Transaction::Eip1559(TxEip1559 {
            chain_id: chain.chain.id(),
            nonce: self.nonce,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: gas_price,
            gas_limit: 1_000_000,
            to: TxKind::Call(to),
            value,
            input: Default::default(),
            access_list: Default::default(),
        });

        let tx_signature_hash = transaction.signature_hash();
        let signature = self.sign_hash(tx_signature_hash);

        // increase nonce for next tx
        self.inc_nonce();

        TransactionSigned::from_transaction_and_signature(transaction, signature)
    }

    /// Sign the transaction hash with the key in memory
    fn sign_hash(&self, hash: B256) -> Signature {
        // let env = std::env::var("WALLET_SECRET_KEY")
        //     .expect("Wallet address is set through environment variable");
        // let secret: B256 = env.parse().expect("WALLET_SECRET_KEY must start with 0x");
        // let secret = B256::from_slice(self.keypair.secret.as_ref());
        let secret = B256::from_slice(&self.keypair.secret_bytes());
        let signature = sign_message(secret, hash);
        signature.expect("failed to sign transaction")
    }

    /// Create and submit the next transaction to the provided [TransactionPool].
    pub async fn create_and_submit_eip1559_pool_tx<Pool>(
        &mut self,
        chain: Arc<ChainSpec>,
        gas_price: u128,
        to: Address,
        value: U256,
        pool: Pool,
    ) -> TxHash
    where
        Pool: TransactionPool,
    {
        let tx = self.create_eip1559(chain, gas_price, to, value);
        let pooled_tx =
            PooledTransactionsElement::try_from_broadcast(tx).expect("tx valid for pool");
        let recovered = pooled_tx.try_into_ecrecovered().expect("tx is recovered");
        let transaction = <Pool::Transaction>::from_recovered_pooled_transaction(recovered);

        pool.add_transaction(TransactionOrigin::Local, transaction)
            .await
            .expect("recovered tx added to pool")
    }

    /// Submit a transaction to the provided pool.
    pub async fn submit_tx_to_pool<Pool>(&self, tx: TransactionSigned, pool: Pool) -> TxHash
    where
        Pool: TransactionPool,
    {
        let pooled_tx =
            PooledTransactionsElement::try_from_broadcast(tx).expect("tx valid for pool");
        let recovered = pooled_tx.try_into_ecrecovered().expect("tx is recovered");
        let transaction = <Pool::Transaction>::from_recovered_pooled_transaction(recovered);

        println!("transaction: \n{transaction:?}\n");

        pool.add_transaction(TransactionOrigin::Local, transaction)
            .await
            .expect("recovered tx added to pool")
    }
}

/// Helper to get the gas price based on the provider's latest header.
pub fn get_gas_price<Provider>(provider: &Provider) -> u128
where
    Provider: BlockReaderIdExt,
{
    let header = provider
        .latest_header()
        .expect("latest header from provider for gas price")
        .expect("latest header is some for gas price");
    header.next_block_base_fee(BaseFeeParams::ethereum()).unwrap_or_default().into()
}

#[cfg(test)]
mod tests {
    use reth_primitives::hex;
    // use std::str::FromStr;

    use super::*;
    #[test]
    fn test_print_key_info() {
        // let mut rng = StdRng::from_seed([0; 32]);
        // let keypair = ExecutionKeypair::generate(&mut rng);

        let secp = Secp256k1::new();
        let (secret_key, _public_key) = secp.generate_keypair(&mut rand::thread_rng());
        let keypair = ExecutionKeypair::from_secret_key(&secp, &secret_key);

        // let private = base64::encode(keypair.secret.as_bytes());
        let secret = keypair.secret_bytes();
        println!("secret: {:?}", hex::encode(secret));
        let pubkey = keypair.public_key().serialize();
        println!("public: {:?}", hex::encode(pubkey));

        // 9bf49a6a0755f953811fce125f2683d50429c3bb49e074147e0089a52eae155f
        // println!("{:?}", hex::encode(bytes));
        // public key hex [0; 32]
        // 029bef8d556d80e43ae7e0becb3a7e6838b95defe45896ed6075bb9035d06c9964
        //
        // let pkey = secp256k1::PublicKey::from_str(
        //     "029bef8d556d80e43ae7e0becb3a7e6838b95defe45896ed6075bb9035d06c9964",
        // )
        // .unwrap();
        // println!("{:?}", public_key_to_address(pkey));
        // println!("pkey: {pkey:?}");
    }
}
