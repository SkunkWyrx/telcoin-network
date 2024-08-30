//! Integration test for RPC Faucet feature.
//!
//! The faucet receives an rpc request containing an address and submits
//! a direct transfer to the address if it is not found in the LRU time-based
//! cache. The signing process is handled by an API call to Google KMS using
//! secp256k1 algorithm. However, additional information is needed for the
//! signature to be EVM compatible. The faucet service does all of this and
//! then submits the transaction to the RPC Transaction Pool for the next batch.

use alloy_sol_types::{sol, SolType};
use alloy::{self, contract::SolCallBuilder};
use gcloud_sdk::{
    google::cloud::{
        deploy,
        kms::v1::{key_management_service_client::KeyManagementServiceClient, GetPublicKeyRequest},
    },
    GoogleApi, GoogleAuthMiddleware, GoogleEnvironment,
};
use jsonrpsee::{core::client::ClientT, rpc_params};
use k256::{elliptic_curve::sec1::ToEncodedPoint, pkcs8::DecodePublicKey, PublicKey as PubKey};
use narwhal_test_utils::{default_test_execution_node, faucet_test_execution_node};
use reth_chainspec::ChainSpec;
use reth_primitives::{
    alloy_primitives::U160, hex, public_key_to_address, Address, GenesisAccount, TransactionSigned, B256, U256
};
use reth_tasks::TaskManager;
use reth_tracing::init_test_tracing;
use secp256k1::PublicKey;
use std::{str::FromStr, sync::Arc, time::Duration};
use tn_faucet::Drip;
use tn_types::{
    adiri_genesis, test_channel, test_utils::{deploy_contract_faucet_initialize, execution_outcome_from_test_batch_, TransactionFactory}, Batch, BatchAPI, NewBatch
};
use tokio::time::timeout;

#[tokio::test]
async fn test_faucet_transfers_tel_with_google_kms() -> eyre::Result<()> {
    init_test_tracing();

    // set application credentials for accessing Google KMS API
    std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", "./gcloud-credentials.json");
    // set Project ID for google_sdk
    std::env::set_var("PROJECT_ID", "telcoin-network");
    // set env vars for faucet cli
    std::env::set_var("KMS_KEY_LOCATIONS", "global");
    std::env::set_var("KMS_KEY_RINGS", "tests");
    std::env::set_var("KMS_CRYPTO_KEYS", "key-for-unit-tests");
    std::env::set_var("KMS_CRYPTO_KEY_VERSIONS", "1");

    // fetch kms address from google and set env
    set_google_kms_public_key_env_var().await?;
    let kms_pem_pubkey = std::env::var("FAUCET_PUBLIC_KEY")?;
    // k256 public key to convert from pem
    let pubkey_from_pem = PubKey::from_public_key_pem(&kms_pem_pubkey)?;
    // secp256k1 public key from uncompressed k256 variation
    let public_key = PublicKey::from_slice(pubkey_from_pem.to_encoded_point(false).as_bytes())?;
    // calculate address from uncompressed public key
    let wallet_address = public_key_to_address(public_key);

    // create genesis and fund account
    let genesis = adiri_genesis();
    let mut tx_factory = TransactionFactory::new();
    // let default_deployer_address = tx_factory.address();
    sol!(
        #[allow(clippy::too_many_arguments)]
        StablecoinManager,
        "../../consensus/types/src/test_utils/artifacts/StablecoinManager.json"
    );
    let faucet_contract_address = Address::random();
    let faucet_bytecode = StablecoinManager::DEPLOYED_BYTECODE.clone();

    let genesis_accounts = vec![
        // (default_deployer_address, GenesisAccount::default().with_balance(U256::MAX)),
        (wallet_address, GenesisAccount::default().with_balance(U256::MAX)),
        (faucet_contract_address, GenesisAccount::default().with_code(Some(faucet_bytecode))),
    ];
    let genesis = genesis.extend_accounts(genesis_accounts.into_iter());
    let chain: Arc<ChainSpec> = Arc::new(genesis.into());

    let manager = TaskManager::current();
    let executor = manager.executor();

    // create engine node
    let execution_node = faucet_test_execution_node(true, Some(chain.clone()), None, executor)?;

    println!("starting batch maker...");
    let worker_id = 0;
    let (to_worker, mut next_batch) = test_channel!(1);

    // start batch maker
    execution_node.start_batch_maker(to_worker, worker_id).await?;
    tracing::info!("getting local address...");
    let unformatted_rpc_url = execution_node
        .worker_http_local_address(&worker_id)
        .await?
        .expect("worker local http address")
        .to_string();
    let rpc_url = format!("http://{}", unformatted_rpc_url);
    tracing::info!("got local address...");

    let client = execution_node.worker_http_client(&worker_id).await?.expect("worker rpc client");
    tracing::info!("got client: {:?}", client);

    let empty_tokens_array = vec![];
    // more than enough time for the nodes to launch RPCs
    let duration = Duration::from_secs(30);
    let deploy_future = async move {
        loop {
            match deploy_contract_faucet_initialize(
                chain.clone(),
                &rpc_url,
                wallet_address,
                empty_tokens_array.clone(),
                &mut tx_factory,
            )
            .await
            {
                Ok(res) => {
                    tracing::info!(?res);
                    return res;
                }
                Err(e) => {
                    tracing::error!("{:?}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    };
    let faucet_contract = timeout(duration, deploy_future).await?;

    // let faucet_contract = deploy_contract_faucet_initialize(chain, &rpc_url, wallet_address,
    // empty_tokens_array.clone(), &mut tx_factory).await?;
    let address = Address::random();

    // assert starting balance is 0
    let starting_balance: String = client.request("eth_getBalance", rpc_params!(address)).await?;
    assert_eq!(U256::from_str(&starting_balance)?, U256::ZERO);

    // note: response is different each time bc KMS
    let tx_hash: String = client.request("faucet_transfer", rpc_params![address]).await?;

    // more than enough time for the next block
    let duration = Duration::from_secs(15);

    // wait for canon event or timeout
    let new_batch: NewBatch = timeout(duration, next_batch.recv()).await?.expect("batch received");

    let batch_txs = new_batch.batch.transactions();
    let tx = batch_txs.first().expect("first batch tx from faucet");
    let recovered = TransactionSigned::decode_enveloped(&mut tx.as_ref())?;

    // assert recovered transaction
    assert_eq!(tx_hash, recovered.hash_ref().to_string());
    assert_eq!(recovered.transaction.to(), Some(faucet_contract));

    // ensure duplicate request is error
    let response =
        client.request::<String, _>("faucet_transfer", rpc_params![faucet_contract]).await;
    Ok(assert!(response.is_err()))
}

#[tokio::test]
async fn test_faucet_transfers_stablecoin_with_google_kms() -> eyre::Result<()> {
    init_test_tracing();

    // set application credentials for accessing Google KMS API
    std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", "./gcloud-credentials.json");
    // set Project ID for google_sdk
    std::env::set_var("PROJECT_ID", "telcoin-network");
    // set env vars for faucet cli
    std::env::set_var("KMS_KEY_LOCATIONS", "global");
    std::env::set_var("KMS_KEY_RINGS", "tests");
    std::env::set_var("KMS_CRYPTO_KEYS", "key-for-unit-tests");
    std::env::set_var("KMS_CRYPTO_KEY_VERSIONS", "1");

    // fetch kms address from google and set env
    set_google_kms_public_key_env_var().await?;
    let kms_pem_pubkey = std::env::var("FAUCET_PUBLIC_KEY")?;
    // k256 public key to convert from pem
    let pubkey_from_pem = PubKey::from_public_key_pem(&kms_pem_pubkey)?;
    // secp256k1 public key from uncompressed k256 variation
    let public_key = PublicKey::from_slice(pubkey_from_pem.to_encoded_point(false).as_bytes())?;
    // calculate address from uncompressed public key
    let wallet_address = public_key_to_address(public_key);

    // create genesis and fund account
    let genesis = adiri_genesis();
    let faucet_account = vec![(wallet_address, GenesisAccount::default().with_balance(U256::MAX))];
    let genesis = genesis.extend_accounts(faucet_account.into_iter());
    let chain = Arc::new(genesis.into());

    let manager = TaskManager::current();
    let executor = manager.executor();

    // create engine node
    let execution_node = faucet_test_execution_node(true, Some(chain), None, executor)?;

    println!("starting batch maker...");
    let worker_id = 0;
    let (to_worker, mut next_batch) = test_channel!(2);

    // start batch maker
    execution_node.start_batch_maker(to_worker, worker_id).await?;
    let unformatted_rpc_url = execution_node.worker_http_local_address(&worker_id);
    // let rpc_url = format!("http://{}", unformatted_rpc_url);

    let user_address = Address::random();
    let client = execution_node.worker_http_client(&worker_id).await?.expect("worker rpc client");

    // assert starting balance is 0
    let starting_balance: String =
        client.request("eth_getBalance", rpc_params!(user_address)).await?;
    assert_eq!(U256::from_str(&starting_balance)?, U256::ZERO);

    let contract_address = Address::from(U160::from(12345678));
    // let faucet = deploy_contract_faucet_initialize(chain, &rpc_url, kms_address,
    // deployed_token_bytes, &mut tx_factory);

    // note: response is different each time bc KMS
    let tx_hash: String =
        client.request("faucet_transfer", rpc_params![user_address, contract_address]).await?;

    // more than enough time for the next block
    let duration = Duration::from_secs(15);

    // wait for canon event or timeout
    let new_batch: NewBatch = timeout(duration, next_batch.recv()).await?.expect("batch received");

    let batch_txs = new_batch.batch.transactions();
    let tx = batch_txs.first().expect("first batch tx from faucet");
    let recovered = TransactionSigned::decode_enveloped(&mut tx.as_ref())?;

    let contract_params: Vec<u8> = Drip::abi_encode_params(&(&contract_address, &user_address));

    // keccak256("Drip(address,address)")[0..4]
    let selector = [235, 56, 57, 167];
    let expected_input = [&selector, &contract_params[..]].concat();

    // assert recovered transaction
    assert_eq!(tx_hash, recovered.hash_ref().to_string());
    assert_eq!(recovered.transaction.input(), &expected_input);

    // ensure duplicate request is error
    let response = client
        .request::<String, _>("faucet_transfer", rpc_params![user_address, contract_address])
        .await;
    assert!(response.is_err());

    // ensure user can request a different stablecoin
    let contract_address = Address::from(U160::from(87654321));
    let response = client
        .request::<String, _>("faucet_transfer", rpc_params![user_address, contract_address])
        .await;
    Ok(assert!(response.is_ok()))
}

/// Keys obtained from google kms calling:
///
/// ```
/// let kms_client: GoogleApi<KeyManagementServiceClient<GoogleAuthMiddleware>> =
///   GoogleApi::from_function(
///     KeyManagementServiceClient::new,
///     "https://cloudkms.googleapis.com",
///     None,
///   )
///   .await?;

/// let validators = [
///   "validator-1",
///   "validator-2",
///   "validator-3",
///   "validator-4",
/// ];

/// let locations = "global";
/// let key_rings = "adiri-testnet";
/// // let crypto_keys = "validator-1";
/// let crypto_key_versions = "1";

/// for v in validators {
///   let name = format!(
///     "projects/{}/locations/{}/keyRings/{}/cryptoKeys/{}/cryptoKeyVersions/{}",
///     google_project_id, locations, key_rings, v, crypto_key_versions
///   );

///   let pubkey = kms_client
///     .get()
///     .get_public_key(tonic::Request::new(GetPublicKeyRequest {
///         name,
///         ..Default::default()
///     }))
///     .await?;

///   println!("{v} public key:\n {:?}", pubkey.into_inner().pem);
/// }
/// ```

#[test]
fn test_print_kms_wallets() -> eyre::Result<()> {
    let keys = [
        "-----BEGIN PUBLIC KEY-----\nMFYwEAYHKoZIzj0CAQYFK4EEAAoDQgAEqzv8pSIJXo3PJZsGv+feaCZJFQoG3ed5\ngl0o/dpBKtwT+yajMYTCravDiqW/g62W+PNVzLoCbaot1WdlwXcp4Q==\n-----END PUBLIC KEY-----\n",
        "-----BEGIN PUBLIC KEY-----\nMFYwEAYHKoZIzj0CAQYFK4EEAAoDQgAEPinA/JiDrvzRDhDEpQU5KysaPZy/d2mv\noJ7fxS90m4tB4weUrBgsy1GeFKSU0TDSW7p9CE+l+36DQiwhkdPyIg==\n-----END PUBLIC KEY-----\n",
        "-----BEGIN PUBLIC KEY-----\nMFYwEAYHKoZIzj0CAQYFK4EEAAoDQgAEwKznkyaaqb/gPgKnmKLzE4EVKOatDdbB\nDrfCV1ofBFFmAGrUxN78HQ27YCKRHhakqRFrIEgnsuIe4KdWhhpoig==\n-----END PUBLIC KEY-----\n",
        "-----BEGIN PUBLIC KEY-----\nMFYwEAYHKoZIzj0CAQYFK4EEAAoDQgAEqUWECyml/Lkvr1QePfIQUH17t/QnSXoj\nNA0o9zB2geizkPIvhMsWB7lzooHI01s1UiPWAGThJb5KvhjNWsqP2g==\n-----END PUBLIC KEY-----\n",
    ];

    for key in keys {
        // k256 public key to convert from pem
        let pubkey_from_pem = PubKey::from_public_key_pem(key)?;
        // secp256k1 public key from uncompressed k256 variation
        let public_key = PublicKey::from_slice(pubkey_from_pem.to_encoded_point(false).as_bytes())?;
        // calculate address from uncompressed public key
        let wallet_address = public_key_to_address(public_key);
        println!("{wallet_address:?}");
    }

    // res: wallets added to genesis
    // 0xe626ce81714cb7777b1bf8ad2323963fb3398ad5
    // 0xb3fabbd1d2edde4d9ced3ce352859ce1bebf7907
    // 0xa3478861957661b2d8974d9309646a71271d98b9
    // 0xe69151677e5aec0b4fc0a94bfcaf20f6f0f975eb

    Ok(())
}

/// Retrieve the public key from KMS.
///
/// This simulates what the startup script should do on deployed nodes:
/// - set an env variable to the PEM formatted key.
async fn set_google_kms_public_key_env_var() -> eyre::Result<()> {
    // Detect Google project ID using environment variables PROJECT_ID/GCP_PROJECT_ID
    // or GKE metadata server when the app runs inside GKE
    let google_project_id = GoogleEnvironment::detect_google_project_id().await
        .expect("No Google Project ID detected. Please specify it explicitly using env variable: PROJECT_ID");

    let kms_client: GoogleApi<KeyManagementServiceClient<GoogleAuthMiddleware>> =
        GoogleApi::from_function(
            KeyManagementServiceClient::new,
            "https://cloudkms.googleapis.com",
            None,
        )
        .await?;

    // retrieve api information from env
    let locations = std::env::var("KMS_KEY_LOCATIONS")
        .expect("KMS_KEY_LOCATIONS must be set in the environment");
    let key_rings =
        std::env::var("KMS_KEY_RINGS").expect("KMS_KEY_RINGS must be set in the environment");
    let crypto_keys =
        std::env::var("KMS_CRYPTO_KEYS").expect("KMS_CRYPTO_KEYS must be set in the environment");
    let crypto_key_versions = std::env::var("KMS_CRYPTO_KEY_VERSIONS")
        .expect("KMS_CRYPTO_KEY_VERSIONS must be set in the environment");

    // construct api endpoint for Google KMS requests
    let name = format!(
        "projects/{}/locations/{}/keyRings/{}/cryptoKeys/{}/cryptoKeyVersions/{}",
        google_project_id, locations, key_rings, crypto_keys, crypto_key_versions
    );

    // request KMS public key
    let kms_pubkey_response =
        kms_client.get().get_public_key(GetPublicKeyRequest { name: name.clone() }).await?;

    // convert pem pubkey format
    let kms_pem_pubkey = kms_pubkey_response.into_inner().pem;
    // store to env
    std::env::set_var("FAUCET_PUBLIC_KEY", kms_pem_pubkey);

    Ok(())
}

async fn get_contract_state_for_genesis() -> eyre::Result<()> {
    let genesis = adiri_genesis();
    let mut tx_factory = TransactionFactory::new();
    sol!(
        #[allow(clippy::too_many_arguments)]
        #[sol(rpc)]
        StablecoinManager,
        "../../consensus/types/src/test_utils/artifacts/StablecoinManager.json"
    );
    let factory_address = tx_factory.address();

    let genesis_accounts = vec![
        (factory_address, GenesisAccount::default().with_balance(U256::MAX)),
    ];
    let genesis = genesis.extend_accounts(genesis_accounts.into_iter());
    let chain: Arc<ChainSpec> = Arc::new(genesis.into());

    let manager = TaskManager::current();
    let executor = manager.executor();

    let execution_node = default_test_execution_node(Some(chain), None, executor)?;
    let provider = execution_node.get_provider().await;
    let block_executor = execution_node.get_block_executor().await;

    // get data for faucet implementation deployment
    let faucet_tx_builder = StablecoinManager::deploy_builder(&provider);
    let faucet_impl = faucet_tx_builder.calculate_create_address();
    let impl_tx_raw = faucet_tx_builder.into_transaction_request().serialize();

    // get data for faucet proxy  deployment w/ initdata
    sol!(
        #[allow(clippy::too_many_arguments)]
        #[sol(rpc)]
        ERC1967Proxy,
        "../../consensus/types/src/test_utils/artifacts/ERC1967Proxy.json"
    );
    let faucet_init_selector = [22, 173, 166, 177];
    let admin: Address = tx_factory.address();
    let deployed_token_bytes = vec![];
    let init_max_limit = U256::MAX;
    let init_min_limit = U256::from(1_000);
    let kms_faucets = vec![kms_address];
    let xyz_amount = U256::from(10).checked_pow(U256::from(6)).expect("1e18 doesn't overflow U256"); // 100 $XYZ
    let tel_amount =
        U256::from(10).checked_pow(U256::from(18)).expect("1e18 doesn't overflow U256"); // 1 $TEL

    let init_params = StablecoinManager::StablecoinManagerInitParams {
        admin_: admin,
        maintainer_: admin,
        tokens_: deployed_token_bytes,
        initMaxLimit: init_max_limit,
        initMinLimit: init_min_limit,
        authorizedFaucets_: kms_faucets,
        dripAmount_: xyz_amount,
        nativeDripAmount_: tel_amount,
    }
    .abi_encode();
    let init_call = [&faucet_init_selector, &init_params[..]].concat().into();
    let faucet_tx_builder = ERC1967Proxy::deploy_builder(&provider, faucet_impl, init_call);
    let faucet_address =  faucet_tx_builder.calculate_create_address();
    let faucet_tx_raw = faucet_tx_builder.into_transaction_request().input(init_call).serialize();
    let faucet_instance = StablecoinManager::new(faucet_address, &provider);
    
    // grant faucet role
    let role_tx_builder: SolCallBuilder<_, _, StablecoinManager::grantRoleCall, _> = 
        faucet_instance.grantRole(
            B256::from_str("0xaecf5761d3ba769b4631978eb26cb84eae66bcaca9c3f0f4ecde3feb2f4cf144")?,
            kms_address,
        );
    let role_tx_raw = role_tx_builder.into_transaction_request().serialize();

    let gas_price = provider.get_gas_price().await?;
    let raw_txs = vec![impl_tx_raw, faucet_tx_raw, role_tx_raw];
    let batch = Batch::new(raw_txs);
    let parent = chain.sealed_genesis_header();
    execution_outcome_from_test_batch_(&batch, &parent, Default::default(), &provider, &block_executor);

    Ok(())
}
