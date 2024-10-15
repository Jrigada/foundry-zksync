use alloy_chains::Chain;
use alloy_dyn_abi::{DynSolValue, JsonAbiExt, Specifier};
use alloy_json_abi::{Constructor, JsonAbi};
use alloy_network::{AnyNetwork, EthereumWallet, TransactionBuilder};
use alloy_primitives::{hex, Address, Bytes};
use alloy_provider::{PendingTransactionError, Provider, ProviderBuilder};
use alloy_rpc_types::{AnyTransactionReceipt, TransactionRequest};
use alloy_serde::WithOtherFields;
use alloy_signer::Signer;
use alloy_transport::{Transport, TransportError};
use clap::{Parser, ValueHint};
use eyre::{Context, Result};
use forge_verify::{zk_provider::CompilerVerificationContext, RetryArgs};
use foundry_cli::{
    opts::{CoreBuildArgs, EthereumOpts, EtherscanOpts, TransactionOpts},
    utils::{self, read_constructor_args_file, remove_contract, remove_zk_contract, LoadConfig},
};
use foundry_common::{
    compile::{self, ProjectCompiler},
    fmt::parse_tokens,
};
use foundry_compilers::{
    artifacts::BytecodeObject, info::ContractInfo, utils::canonicalize,
    zksync::artifact_output::zk::ZkContractArtifact,
};
use foundry_config::{
    figment::{
        self,
        value::{Dict, Map},
        Metadata, Profile,
    },
    merge_impl_figment_convert, Config,
};
use foundry_wallets::WalletSigner;
use foundry_zksync_core::convert::ConvertH160;
use serde_json::json;
use std::{
    borrow::Borrow,
    collections::{HashSet, VecDeque},
    marker::PhantomData,
    path::PathBuf,
    str::FromStr,
    sync::Arc,
};
use zksync_types::H256;
use zksync_web3_rs::eip712::PaymasterParams;

merge_impl_figment_convert!(CreateArgs, opts, eth);

/// CLI arguments for `forge create`.
#[derive(Clone, Debug, Parser)]
pub struct CreateArgs {
    /// The contract identifier in the form `<path>:<contractname>`.
    contract: ContractInfo,

    /// The constructor arguments.
    #[arg(
        long,
        num_args(1..),
        conflicts_with = "constructor_args_path",
        value_name = "ARGS",
    )]
    constructor_args: Vec<String>,

    /// The path to a file containing the constructor arguments.
    #[arg(
        long,
        value_hint = ValueHint::FilePath,
        value_name = "PATH",
        conflicts_with = "constructor_args",
    )]
    constructor_args_path: Option<PathBuf>,

    /// Print the deployment information as JSON.
    #[arg(long, help_heading = "Display options")]
    json: bool,

    /// Verify contract after creation.
    #[arg(long)]
    verify: bool,

    /// Send via `eth_sendTransaction` using the `--from` argument or `$ETH_FROM` as sender
    #[arg(long, requires = "from")]
    unlocked: bool,

    /// Prints the standard json compiler input if `--verify` is provided.
    ///
    /// The standard json compiler input can be used to manually submit contract verification in
    /// the browser.
    #[arg(long, requires = "verify")]
    show_standard_json_input: bool,

    /// Timeout to use for broadcasting transactions.
    #[arg(long, env = "ETH_TIMEOUT")]
    pub timeout: Option<u64>,

    #[command(flatten)]
    opts: CoreBuildArgs,

    #[command(flatten)]
    tx: TransactionOpts,

    #[command(flatten)]
    eth: EthereumOpts,

    #[command(flatten)]
    pub verifier: forge_verify::VerifierArgs,

    #[command(flatten)]
    retry: RetryArgs,
}

/// Data used to deploy a contract on zksync
pub struct ZkSyncData {
    bytecode: Vec<u8>,
    bytecode_hash: H256,
    factory_deps: Vec<Vec<u8>>,
    paymaster_params: Option<PaymasterParams>,
}

impl CreateArgs {
    /// Executes the command to create a contract
    pub async fn run(mut self) -> Result<()> {
        let config = self.try_load_config_emit_warnings()?;
        // Find Project & Compile
        let project = config.project()?;

        let zksync = self.opts.compiler.zk.enabled();
        if zksync {
            let paymaster_params =
                if let Some(paymaster_address) = self.opts.compiler.zk.paymaster_address {
                    Some(PaymasterParams {
                        paymaster: paymaster_address,
                        paymaster_input: self
                            .opts
                            .compiler
                            .zk
                            .paymaster_input
                            .clone()
                            .unwrap_or_default()
                            .to_vec(),
                    })
                } else {
                    None
                };
            let target_path = if let Some(ref mut path) = self.contract.path {
                canonicalize(project.root().join(path))?
            } else {
                project.find_contract_path(&self.contract.name)?
            };

            let config = self.opts.try_load_config_emit_warnings()?;
            let zk_project =
                foundry_zksync_compiler::config_create_project(&config, config.cache, false)?;
            let zk_compiler = ProjectCompiler::new()
                .quiet(self.json || self.opts.silent)
                .files([target_path.clone()]);
            let mut zk_output =
                zk_compiler.zksync_compile(&zk_project, config.zksync.avoid_contracts())?;

            let artifact = remove_zk_contract(&mut zk_output, &target_path, &self.contract.name)?;

            let ZkContractArtifact { bytecode, hash, factory_dependencies, abi, .. } = artifact;

            let abi = abi.expect("Abi not found");

            let bin = bytecode.expect("Bytecode not found");
            let bytecode_hash = H256::from_str(&hash.expect("Contract hash not found"))?;
            let bytecode = bin.object.clone().into_bytes().unwrap().to_vec();

            // Add arguments to constructor
            let config = self.eth.try_load_config_emit_warnings()?;
            let provider = utils::get_provider(&config)?;
            let params = match abi.constructor {
                Some(ref v) => {
                    let constructor_args =
                        if let Some(ref constructor_args_path) = self.constructor_args_path {
                            read_constructor_args_file(constructor_args_path.to_path_buf())?
                        } else {
                            self.constructor_args.clone()
                        };
                    self.parse_constructor_args(v, &constructor_args)?
                }
                None => vec![],
            };

            // respect chain, if set explicitly via cmd args
            let chain_id = if let Some(chain_id) = self.chain_id() {
                chain_id
            } else {
                provider.get_chain_id().await?
            };

            let factory_deps: Vec<Vec<u8>> = {
                let factory_dependencies_map =
                    factory_dependencies.expect("factory deps not found");
                let mut visited_paths = HashSet::new();
                let mut visited_bytecodes = HashSet::new();
                let mut queue = VecDeque::new();

                for dep in factory_dependencies_map.values() {
                    queue.push_back(dep.clone());
                }

                while let Some(dep_info) = queue.pop_front() {
                    if visited_paths.insert(dep_info.clone()) {
                        let mut split = dep_info.split(':');
                        let contract_path = split
                            .next()
                            .expect("Failed to extract contract path for factory dependency");
                        let contract_name = split
                            .next()
                            .expect("Failed to extract contract name for factory dependency");
                        let mut abs_path_buf = PathBuf::new();
                        abs_path_buf.push(project.root());
                        abs_path_buf.push(contract_path);
                        let fdep_art =
                            zk_output.find(&abs_path_buf, contract_name).unwrap_or_else(|| {
                                panic!(
                                    "Could not find contract {contract_name} at path {contract_path} for compilation output",
                                )
                            });
                        let fdep_fdeps_map =
                            fdep_art.factory_dependencies.clone().expect("factory deps not found");
                        for dep in fdep_fdeps_map.values() {
                            queue.push_back(dep.clone())
                        }

                        let fdep_bytecode = fdep_art
                            .bytecode
                            .clone()
                            .expect("Bytecode not found for factory dependency")
                            .object
                            .clone()
                            .into_bytes()
                            .unwrap()
                            .to_vec();
                        visited_bytecodes.insert(fdep_bytecode);
                    }
                }
                visited_bytecodes.insert(bytecode.clone());
                visited_bytecodes.into_iter().collect()
            };
            let zk_data = ZkSyncData { bytecode, bytecode_hash, factory_deps, paymaster_params };

            let result = if self.unlocked {
                // Deploy with unlocked account
                let sender = self.eth.wallet.from.expect("required");
                self.deploy_zk(
                    abi,
                    bin.object,
                    params,
                    provider,
                    chain_id,
                    sender,
                    config.transaction_timeout,
                    zk_data,
                    None,
                )
                .await
            } else {
                // Deploy with signer
                let signer = self.eth.wallet.signer().await?;
                let zk_signer = self.eth.wallet.signer().await?;
                let deployer = signer.address();
                let provider = ProviderBuilder::<_, _, AnyNetwork>::default()
                    .wallet(EthereumWallet::new(signer))
                    .on_provider(provider);
                self.deploy_zk(
                    abi,
                    bin.object,
                    params,
                    provider,
                    chain_id,
                    deployer,
                    config.transaction_timeout,
                    zk_data,
                    Some(zk_signer),
                )
                .await
            };

            return result;
        }

        let target_path = if let Some(ref mut path) = self.contract.path {
            canonicalize(project.root().join(path))?
        } else {
            project.find_contract_path(&self.contract.name)?
        };

        let mut output =
            compile::compile_target(&target_path, &project, self.json || self.opts.silent)?;

        let (abi, bin, _) = remove_contract(&mut output, &target_path, &self.contract.name)?;

        let bin = match bin.object {
            BytecodeObject::Bytecode(_) => bin.object,
            _ => {
                let link_refs = bin
                    .link_references
                    .iter()
                    .flat_map(|(path, names)| {
                        names.keys().map(move |name| format!("\t{name}: {path}"))
                    })
                    .collect::<Vec<String>>()
                    .join("\n");
                eyre::bail!("Dynamic linking not supported in `create` command - deploy the following library contracts first, then provide the address to link at compile time\n{}", link_refs)
            }
        };

        // Add arguments to constructor
        let provider = utils::get_provider(&config)?;
        let params = match abi.constructor {
            Some(ref v) => {
                let constructor_args =
                    if let Some(ref constructor_args_path) = self.constructor_args_path {
                        read_constructor_args_file(constructor_args_path.to_path_buf())?
                    } else {
                        self.constructor_args.clone()
                    };
                self.parse_constructor_args(v, &constructor_args)?
            }
            None => vec![],
        };

        // respect chain, if set explicitly via cmd args
        let chain_id = if let Some(chain_id) = self.chain_id() {
            chain_id
        } else {
            provider.get_chain_id().await?
        };
        if self.unlocked {
            // Deploy with unlocked account
            let sender = self.eth.wallet.from.expect("required");
            self.deploy(abi, bin, params, provider, chain_id, sender, config.transaction_timeout)
                .await
        } else {
            // Deploy with signer
            let signer = self.eth.wallet.signer().await?;
            let deployer = signer.address();
            let provider = ProviderBuilder::<_, _, AnyNetwork>::default()
                .wallet(EthereumWallet::new(signer))
                .on_provider(provider);
            self.deploy(abi, bin, params, provider, chain_id, deployer, config.transaction_timeout)
                .await
        }
    }

    /// Returns the provided chain id, if any.
    fn chain_id(&self) -> Option<u64> {
        self.eth.etherscan.chain.map(|chain| chain.id())
    }

    /// Ensures the verify command can be executed.
    ///
    /// This is supposed to check any things that might go wrong when preparing a verify request
    /// before the contract is deployed. This should prevent situations where a contract is deployed
    /// successfully, but we fail to prepare a verify request which would require manual
    /// verification.
    async fn verify_preflight_check(
        &self,
        constructor_args: Option<String>,
        chain: u64,
    ) -> Result<()> {
        // NOTE: this does not represent the same `VerifyArgs` that would be sent after deployment,
        // since we don't know the address yet.
        let mut verify = forge_verify::VerifyArgs {
            address: Default::default(),
            contract: Some(self.contract.clone()),
            compiler_version: None,
            constructor_args,
            constructor_args_path: None,
            num_of_optimizations: None,
            etherscan: EtherscanOpts {
                key: self.eth.etherscan.key.clone(),
                chain: Some(chain.into()),
            },
            rpc: Default::default(),
            flatten: false,
            force: false,
            skip_is_verified_check: true,
            watch: true,
            retry: self.retry,
            libraries: self.opts.libraries.clone(),
            root: None,
            verifier: self.verifier.clone(),
            via_ir: self.opts.via_ir,
            evm_version: self.opts.compiler.evm_version,
            show_standard_json_input: self.show_standard_json_input,
            guess_constructor_args: false,
            zksync: self.opts.compiler.zk.enabled(),
        };

        // Check config for Etherscan API Keys to avoid preflight check failing if no
        // ETHERSCAN_API_KEY value set.
        let config = verify.load_config_emit_warnings();
        verify.etherscan.key =
            config.get_etherscan_config_with_chain(Some(chain.into()))?.map(|c| c.key);

        let context = if verify.zksync {
            CompilerVerificationContext::ZkSolc(verify.zk_resolve_context().await?)
        } else {
            CompilerVerificationContext::Solc(verify.resolve_context().await?)
        };

        verify.verification_provider()?.preflight_verify_check(verify, context).await?;
        Ok(())
    }

    /// Deploys the contract
    #[allow(clippy::too_many_arguments)]
    async fn deploy<P: Provider<T, AnyNetwork>, T: Transport + Clone>(
        self,
        abi: JsonAbi,
        bin: BytecodeObject,
        args: Vec<DynSolValue>,
        provider: P,
        chain: u64,
        deployer_address: Address,
        timeout: u64,
    ) -> Result<()> {
        let bin = bin.into_bytes().unwrap_or_else(|| {
            panic!("no bytecode found in bin object for {}", self.contract.name)
        });
        let provider = Arc::new(provider);
        let factory = ContractFactory::new(abi.clone(), bin.clone(), provider.clone(), timeout);

        let is_args_empty = args.is_empty();
        let mut deployer =
            factory.deploy_tokens(args.clone()).context("failed to deploy contract").map_err(|e| {
                if is_args_empty {
                    e.wrap_err("no arguments provided for contract constructor; consider --constructor-args or --constructor-args-path")
                } else {
                    e
                }
            })?;
        let is_legacy = self.tx.legacy || Chain::from(chain).is_legacy();

        deployer.tx.set_from(deployer_address);
        deployer.tx.set_chain_id(chain);
        // `to` field must be set explicitly, cannot be None.
        if deployer.tx.to.is_none() {
            deployer.tx.set_create();
        }
        deployer.tx.set_nonce(if let Some(nonce) = self.tx.nonce {
            Ok(nonce.to())
        } else {
            provider.get_transaction_count(deployer_address).await
        }?);

        // set tx value if specified
        if let Some(value) = self.tx.value {
            deployer.tx.set_value(value);
        }

        deployer.tx.set_gas_limit(if let Some(gas_limit) = self.tx.gas_limit {
            Ok(gas_limit.to())
        } else {
            provider.estimate_gas(&deployer.tx).await
        }?);

        if is_legacy {
            let gas_price = if let Some(gas_price) = self.tx.gas_price {
                gas_price.to()
            } else {
                provider.get_gas_price().await?
            };
            deployer.tx.set_gas_price(gas_price);
        } else {
            let estimate = provider.estimate_eip1559_fees(None).await.wrap_err("Failed to estimate EIP1559 fees. This chain might not support EIP1559, try adding --legacy to your command.")?;
            let priority_fee = if let Some(priority_fee) = self.tx.priority_gas_price {
                priority_fee.to()
            } else {
                estimate.max_priority_fee_per_gas
            };
            let max_fee = if let Some(max_fee) = self.tx.gas_price {
                max_fee.to()
            } else {
                estimate.max_fee_per_gas
            };

            deployer.tx.set_max_fee_per_gas(max_fee);
            deployer.tx.set_max_priority_fee_per_gas(priority_fee);
        }

        // Before we actually deploy the contract we try check if the verify settings are valid
        let mut constructor_args = None;
        if self.verify {
            if !args.is_empty() {
                let encoded_args = abi
                    .constructor()
                    .ok_or_else(|| eyre::eyre!("could not find constructor"))?
                    .abi_encode_input(&args)?;
                constructor_args = Some(hex::encode_prefixed(encoded_args));
            }

            self.verify_preflight_check(constructor_args.clone(), chain).await?;
        }

        // Deploy the actual contract
        let (deployed_contract, receipt) = deployer.send_with_receipt().await?;

        let address = deployed_contract;
        if self.json {
            let output = json!({
                "deployer": deployer_address.to_string(),
                "deployedTo": address.to_string(),
                "transactionHash": receipt.transaction_hash
            });
            println!("{output}");
        } else {
            println!("Deployer: {deployer_address}");
            println!("Deployed to: {address}");
            println!("Transaction hash: {:?}", receipt.transaction_hash);
        };

        if !self.verify {
            return Ok(());
        }

        println!("Starting contract verification...");

        let num_of_optimizations =
            if self.opts.compiler.optimize { self.opts.compiler.optimizer_runs } else { None };
        let verify = forge_verify::VerifyArgs {
            address,
            contract: Some(self.contract),
            compiler_version: None,
            constructor_args,
            constructor_args_path: None,
            num_of_optimizations,
            etherscan: EtherscanOpts { key: self.eth.etherscan.key(), chain: Some(chain.into()) },
            rpc: Default::default(),
            flatten: false,
            force: false,
            skip_is_verified_check: false,
            watch: true,
            retry: self.retry,
            libraries: self.opts.libraries.clone(),
            root: None,
            verifier: self.verifier,
            via_ir: self.opts.via_ir,
            evm_version: self.opts.compiler.evm_version,
            show_standard_json_input: self.show_standard_json_input,
            guess_constructor_args: false,
            zksync: self.opts.compiler.zk.enabled(),
        };
        println!("Waiting for {} to detect contract deployment...", verify.verifier.verifier);
        verify.run().await
    }

    // Deploys the zk contract
    #[allow(clippy::too_many_arguments)]
    async fn deploy_zk<P: Provider<T, AnyNetwork>, T: Transport + Clone>(
        self,
        abi: JsonAbi,
        bin: BytecodeObject,
        args: Vec<DynSolValue>,
        provider: P,
        chain: u64,
        deployer_address: Address,
        timeout: u64,
        zk_data: ZkSyncData,
        zk_signer: Option<WalletSigner>,
    ) -> Result<()> {
        let bin = bin.into_bytes().unwrap_or_else(|| {
            panic!("no bytecode found in bin object for {}", self.contract.name)
        });
        let provider = Arc::new(provider);
        let factory = ContractFactory::new(abi.clone(), bin.clone(), provider.clone(), timeout);

        let is_args_empty = args.is_empty();
        let mut deployer =
            factory.deploy_tokens_zk(args.clone(), &zk_data).context("failed to deploy contract")
                .map(|deployer| deployer.set_zk_factory_deps(zk_data.factory_deps.clone()).set_zk_paymaster_params(zk_data.paymaster_params.clone())).map_err(|e| {
                if is_args_empty {
                    e.wrap_err("no arguments provided for contract constructor; consider --constructor-args or --constructor-args-path")
                } else {
                    e
                }
            })?;

        deployer.tx.set_from(deployer_address);
        deployer.tx.set_chain_id(chain);
        // `to` field must be set explicitly, cannot be None.
        if deployer.tx.to.is_none() {
            deployer.tx.set_create();
        }
        deployer.tx.set_nonce(if let Some(nonce) = self.tx.nonce {
            Ok(nonce.to())
        } else {
            provider.get_transaction_count(deployer_address).await
        }?);

        // set tx value if specified
        if let Some(value) = self.tx.value {
            deployer.tx.set_value(value);
        }

        let gas_price = if let Some(gas_price) = self.tx.gas_price {
            gas_price.to()
        } else {
            provider.get_gas_price().await?
        };
        deployer.tx.set_gas_price(gas_price);

        let estimated_gas = foundry_zksync_core::estimate_gas(
            &deployer.tx,
            zk_data.factory_deps.clone(),
            &provider,
        )
        .await?;

        deployer.tx.set_gas_limit(if let Some(gas_limit) = self.tx.gas_limit {
            gas_limit.to::<u128>()
        } else {
            estimated_gas.limit
        });

        let zk_constructor_args = match abi.constructor() {
            None => Default::default(),
            Some(constructor) => constructor.abi_encode_input(&args).unwrap_or_default(),
        };
        let data = foundry_zksync_core::encode_create_params(
            &forge::revm::primitives::CreateScheme::Create,
            zk_data.bytecode_hash,
            zk_constructor_args,
        );
        let data = Bytes::from(data);
        deployer.tx.set_input(data);

        deployer.tx.set_to(foundry_zksync_core::CONTRACT_DEPLOYER_ADDRESS.to_address());

        let mut constructor_args = None;
        if self.verify {
            if !args.is_empty() {
                let encoded_args = abi
                    .constructor()
                    .ok_or_else(|| eyre::eyre!("could not find constructor"))?
                    .abi_encode_input(&args)?;
                constructor_args = Some(hex::encode(encoded_args));
            }

            self.verify_preflight_check(constructor_args.clone(), chain).await?;
        }

        // Deploy the actual contract
        let (deployed_contract, receipt) = deployer.send_with_receipt_zk(zk_signer).await?;

        let address = deployed_contract;
        if self.json {
            let output = json!({
                "deployer": deployer_address.to_string(),
                "deployedTo": address.to_string(),
                "transactionHash": receipt.transaction_hash
            });
            println!("{output}");
        } else {
            println!("Deployer: {deployer_address}");
            println!("Deployed to: {address}");
            println!("Transaction hash: {:?}", receipt.transaction_hash);
        };

        if !self.verify {
            return Ok(());
        }

        println!("Starting contract verification...");

        let num_of_optimizations =
            if self.opts.compiler.optimize { self.opts.compiler.optimizer_runs } else { None };
        let verify = forge_verify::VerifyArgs {
            address,
            contract: Some(self.contract),
            compiler_version: None,
            constructor_args,
            constructor_args_path: None,
            num_of_optimizations,
            etherscan: EtherscanOpts { key: self.eth.etherscan.key(), chain: Some(chain.into()) },
            rpc: Default::default(),
            flatten: false,
            force: false,
            skip_is_verified_check: false,
            watch: true,
            retry: self.retry,
            libraries: self.opts.libraries.clone(),
            root: None,
            verifier: self.verifier,
            via_ir: self.opts.via_ir,
            evm_version: self.opts.compiler.evm_version,
            show_standard_json_input: self.show_standard_json_input,
            guess_constructor_args: false,
            zksync: true,
        };
        println!("Waiting for {} to detect contract deployment...", verify.verifier.verifier);
        verify.run().await
    }

    /// Parses the given constructor arguments into a vector of `DynSolValue`s, by matching them
    /// against the constructor's input params.
    ///
    /// Returns a list of parsed values that match the constructor's input params.
    fn parse_constructor_args(
        &self,
        constructor: &Constructor,
        constructor_args: &[String],
    ) -> Result<Vec<DynSolValue>> {
        let expected_params = constructor.inputs.len();

        let mut params = Vec::with_capacity(expected_params);
        for (input, arg) in constructor.inputs.iter().zip(constructor_args) {
            // resolve the input type directly
            let ty = input
                .resolve()
                .wrap_err_with(|| format!("Could not resolve constructor arg: input={input}"))?;
            params.push((ty, arg));
        }

        let actual_params = params.len();

        if actual_params != expected_params {
            tracing::warn!(
                given = actual_params,
                expected = expected_params,
               "Constructor argument mismatch: expected {expected_params} arguments, but received {actual_params}. Ensure that the number of arguments provided matches the constructor definition."
            );
        }

        let params = params.iter().map(|(ty, arg)| (ty, arg.as_str()));
        parse_tokens(params).map_err(Into::into)
    }
}

impl figment::Provider for CreateArgs {
    fn metadata(&self) -> Metadata {
        Metadata::named("Create Args Provider")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        let mut dict = Dict::default();
        if let Some(timeout) = self.timeout {
            dict.insert("transaction_timeout".to_string(), timeout.into());
        }
        Ok(Map::from([(Config::selected_profile(), dict)]))
    }
}

/// `ContractFactory` is a [`DeploymentTxFactory`] object with an
/// [`Arc`] middleware. This type alias exists to preserve backwards
/// compatibility with less-abstract Contracts.
///
/// For full usage docs, see [`DeploymentTxFactory`].
pub type ContractFactory<P, T> = DeploymentTxFactory<Arc<P>, P, T>;

/// Helper which manages the deployment transaction of a smart contract. It
/// wraps a deployment transaction, and retrieves the contract address output
/// by it.
///
/// Currently, we recommend using the [`ContractDeployer`] type alias.
#[derive(Debug)]
#[must_use = "ContractDeploymentTx does nothing unless you `send` it"]
pub struct ContractDeploymentTx<B, P, T, C> {
    /// the actual deployer, exposed for overriding the defaults
    pub deployer: Deployer<B, P, T>,
    /// marker for the `Contract` type to create afterwards
    ///
    /// this type will be used to construct it via `From::from(Contract)`
    _contract: PhantomData<C>,
}

impl<B, P, T, C> Clone for ContractDeploymentTx<B, P, T, C>
where
    B: Clone,
{
    fn clone(&self) -> Self {
        Self { deployer: self.deployer.clone(), _contract: self._contract }
    }
}

impl<B, P, T, C> From<Deployer<B, P, T>> for ContractDeploymentTx<B, P, T, C> {
    fn from(deployer: Deployer<B, P, T>) -> Self {
        Self { deployer, _contract: PhantomData }
    }
}

/// Helper which manages the deployment transaction of a smart contract
#[derive(Debug)]
#[must_use = "Deployer does nothing unless you `send` it"]
pub struct Deployer<B, P, T> {
    /// The deployer's transaction, exposed for overriding the defaults
    pub tx: WithOtherFields<TransactionRequest>,
    abi: JsonAbi,
    client: B,
    confs: usize,
    timeout: u64,
    zk_factory_deps: Option<Vec<Vec<u8>>>,
    zk_paymaster_params: Option<PaymasterParams>,
    _p: PhantomData<P>,
    _t: PhantomData<T>,
}

impl<B, P, T> Clone for Deployer<B, P, T>
where
    B: Clone,
{
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            abi: self.abi.clone(),
            client: self.client.clone(),
            confs: self.confs,
            timeout: self.timeout,
            zk_factory_deps: self.zk_factory_deps.clone(),
            zk_paymaster_params: self.zk_paymaster_params.clone(),
            _p: PhantomData,
            _t: PhantomData,
        }
    }
}

impl<B, P, T> Deployer<B, P, T>
where
    B: Borrow<P> + Clone,
    P: Provider<T, AnyNetwork>,
    T: Transport + Clone,
{
    /// Set zksync's factory deps.
    pub fn set_zk_factory_deps(mut self, deps: Vec<Vec<u8>>) -> Self {
        self.zk_factory_deps = Some(deps);
        self
    }

    /// Set zksync's paymaster params.
    pub fn set_zk_paymaster_params(mut self, params: Option<PaymasterParams>) -> Self {
        self.zk_paymaster_params = params;
        self
    }

    /// Broadcasts the zk contract deployment transaction and after waiting for it to
    /// be sufficiently confirmed (default: 1), it returns a tuple with
    /// the [`Contract`](crate::Contract) struct at the deployed contract's address
    /// and the corresponding [`AnyReceipt`].
    pub async fn send_with_receipt_zk(
        self,
        signer: Option<WalletSigner>,
    ) -> Result<(Address, AnyTransactionReceipt), ContractDeploymentError> {
        let factory_deps = self.zk_factory_deps.unwrap_or_default();
        let tx = foundry_zksync_core::new_eip712_transaction(
            self.tx,
            factory_deps,
            self.zk_paymaster_params.clone(),
            self.client.borrow(),
            signer.expect("No signer was found"),
        )
        .await
        .map_err(|_| ContractDeploymentError::ContractNotDeployed)?;
        let receipt = self
            .client
            .borrow()
            .send_raw_transaction(&tx)
            .await?
            .with_required_confirmations(self.confs as u64)
            .get_receipt()
            .await?;

        let address =
            receipt.contract_address.ok_or(ContractDeploymentError::ContractNotDeployed)?;

        Ok((address, receipt))
    }

    /// Broadcasts the contract deployment transaction and after waiting for it to
    /// be sufficiently confirmed (default: 1), it returns a tuple with
    /// the [`Contract`](crate::Contract) struct at the deployed contract's address
    /// and the corresponding [`AnyReceipt`].
    pub async fn send_with_receipt(
        self,
    ) -> Result<(Address, AnyTransactionReceipt), ContractDeploymentError> {
        let receipt = self
            .client
            .borrow()
            .send_transaction(self.tx)
            .await?
            .with_required_confirmations(self.confs as u64)
            .get_receipt()
            .await?;

        let address =
            receipt.contract_address.ok_or(ContractDeploymentError::ContractNotDeployed)?;

        Ok((address, receipt))
    }
}

/// To deploy a contract to the Ethereum network, a `ContractFactory` can be
/// created which manages the Contract bytecode and Application Binary Interface
/// (ABI), usually generated from the Solidity compiler.
///
/// Once the factory's deployment transaction is mined with sufficient confirmations,
/// the [`Contract`](crate::Contract) object is returned.
///
/// # Example
///
/// ```
/// # async fn foo() -> Result<(), Box<dyn std::error::Error>> {
/// use alloy_primitives::Bytes;
/// use ethers_contract::ContractFactory;
/// use ethers_providers::{Provider, Http};
///
/// // get the contract ABI and bytecode
/// let abi = Default::default();
/// let bytecode = Bytes::from_static(b"...");
///
/// // connect to the network
/// let client = Provider::<Http>::try_from("http://localhost:8545").unwrap();
/// let client = std::sync::Arc::new(client);
///
/// // create a factory which will be used to deploy instances of the contract
/// let factory = ContractFactory::new(abi, bytecode, client);
///
/// // The deployer created by the `deploy` call exposes a builder which gets consumed
/// // by the async `send` call
/// let contract = factory
///     .deploy("initial value".to_string())?
///     .confirmations(0usize)
///     .send()
///     .await?;
/// println!("{}", contract.address());
/// # Ok(())
/// # }
#[derive(Debug)]
pub struct DeploymentTxFactory<B, P, T> {
    client: B,
    abi: JsonAbi,
    bytecode: Bytes,
    timeout: u64,
    _p: PhantomData<P>,
    _t: PhantomData<T>,
}

impl<B, P, T> Clone for DeploymentTxFactory<B, P, T>
where
    B: Clone,
{
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            abi: self.abi.clone(),
            bytecode: self.bytecode.clone(),
            timeout: self.timeout,
            _p: PhantomData,
            _t: PhantomData,
        }
    }
}

impl<P, T, B> DeploymentTxFactory<B, P, T>
where
    B: Borrow<P> + Clone,
    P: Provider<T, AnyNetwork>,
    T: Transport + Clone,
{
    /// Creates a factory for deployment of the Contract with bytecode, and the
    /// constructor defined in the abi. The client will be used to send any deployment
    /// transaction.
    pub fn new(abi: JsonAbi, bytecode: Bytes, client: B, timeout: u64) -> Self {
        Self { client, abi, bytecode, timeout, _p: PhantomData, _t: PhantomData }
    }

    /// Create a deployment tx using the provided tokens as constructor
    /// arguments
    pub fn deploy_tokens(
        self,
        params: Vec<DynSolValue>,
    ) -> Result<Deployer<B, P, T>, ContractDeploymentError>
    where
        B: Clone,
    {
        // Encode the constructor args & concatenate with the bytecode if necessary
        let data: Bytes = match (self.abi.constructor(), params.is_empty()) {
            (None, false) => return Err(ContractDeploymentError::ConstructorError),
            (None, true) => self.bytecode.clone(),
            (Some(constructor), _) => {
                let input: Bytes = constructor
                    .abi_encode_input(&params)
                    .map_err(ContractDeploymentError::DetokenizationError)?
                    .into();
                // Concatenate the bytecode and abi-encoded constructor call.
                self.bytecode.iter().copied().chain(input).collect()
            }
        };

        // create the tx object. Since we're deploying a contract, `to` is `None`
        let tx = WithOtherFields::new(TransactionRequest::default().input(data.into()));

        Ok(Deployer {
            client: self.client.clone(),
            abi: self.abi,
            tx,
            confs: 1,
            timeout: self.timeout,
            zk_factory_deps: None,
            zk_paymaster_params: None,
            _p: PhantomData,
            _t: PhantomData,
        })
    }

    /// Create a deployment tx using the provided tokens as constructor
    /// arguments
    pub fn deploy_tokens_zk(
        self,
        params: Vec<DynSolValue>,
        zk_data: &ZkSyncData,
    ) -> Result<Deployer<B, P, T>, ContractDeploymentError>
    where
        B: Clone,
    {
        if self.abi.constructor().is_none() && !params.is_empty() {
            return Err(ContractDeploymentError::ConstructorError)
        }

        // Encode the constructor args & concatenate with the bytecode if necessary
        let constructor_args = match self.abi.constructor() {
            None => Default::default(),
            Some(constructor) => constructor.abi_encode_input(&params).unwrap_or_default(),
        };
        let data: Bytes = foundry_zksync_core::encode_create_params(
            &forge::revm::primitives::CreateScheme::Create,
            zk_data.bytecode_hash,
            constructor_args,
        )
        .into();

        // create the tx object.
        let tx = WithOtherFields::new(
            TransactionRequest::default()
                .to(foundry_zksync_core::CONTRACT_DEPLOYER_ADDRESS.to_address())
                .input(data.into()),
        );
        Ok(Deployer {
            client: self.client.clone(),
            abi: self.abi,
            tx,
            confs: 1,
            timeout: self.timeout,
            zk_factory_deps: Some(vec![zk_data.bytecode.clone()]),
            zk_paymaster_params: zk_data.paymaster_params.clone(),
            _p: PhantomData,
            _t: PhantomData,
        })
    }
}

#[derive(thiserror::Error, Debug)]
/// An Error which is thrown when interacting with a smart contract
pub enum ContractDeploymentError {
    #[error("constructor is not defined in the ABI")]
    ConstructorError,
    #[error(transparent)]
    DetokenizationError(#[from] alloy_dyn_abi::Error),
    #[error("contract was not deployed")]
    ContractNotDeployed,
    #[error(transparent)]
    RpcError(#[from] TransportError),
}

impl From<PendingTransactionError> for ContractDeploymentError {
    fn from(_err: PendingTransactionError) -> Self {
        Self::ContractNotDeployed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_parse_create() {
        let args: CreateArgs = CreateArgs::parse_from([
            "foundry-cli",
            "src/Domains.sol:Domains",
            "--verify",
            "--retries",
            "10",
            "--delay",
            "30",
        ]);
        assert_eq!(args.retry.retries, 10);
        assert_eq!(args.retry.delay, 30);
    }
    #[test]
    fn can_parse_chain_id() {
        let args: CreateArgs = CreateArgs::parse_from([
            "foundry-cli",
            "src/Domains.sol:Domains",
            "--verify",
            "--retries",
            "10",
            "--delay",
            "30",
            "--chain-id",
            "9999",
        ]);
        assert_eq!(args.chain_id(), Some(9999));
    }

    #[test]
    fn test_parse_constructor_args() {
        let args: CreateArgs = CreateArgs::parse_from([
            "foundry-cli",
            "src/Domains.sol:Domains",
            "--constructor-args",
            "Hello",
        ]);
        let constructor: Constructor = serde_json::from_str(r#"{"type":"constructor","inputs":[{"name":"_name","type":"string","internalType":"string"}],"stateMutability":"nonpayable"}"#).unwrap();
        let params = args.parse_constructor_args(&constructor, &args.constructor_args).unwrap();
        assert_eq!(params, vec![DynSolValue::String("Hello".to_string())]);
    }

    #[test]
    fn test_parse_tuple_constructor_args() {
        let args: CreateArgs = CreateArgs::parse_from([
            "foundry-cli",
            "src/Domains.sol:Domains",
            "--constructor-args",
            "[(1,2), (2,3), (3,4)]",
        ]);
        let constructor: Constructor = serde_json::from_str(r#"{"type":"constructor","inputs":[{"name":"_points","type":"tuple[]","internalType":"struct Point[]","components":[{"name":"x","type":"uint256","internalType":"uint256"},{"name":"y","type":"uint256","internalType":"uint256"}]}],"stateMutability":"nonpayable"}"#).unwrap();
        let _params = args.parse_constructor_args(&constructor, &args.constructor_args).unwrap();
    }
}
