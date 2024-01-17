//! Genesis helper methods.
//!
//! The yaml, chainspec, and Genesis struct are used for all
//! testing purposes.
//!
//! Yukon is the current name for multi-node testnet.

use crate::{
    verify_proof_of_possession, BlsPublicKey, BlsSignature, Committee, CommitteeBuilder, Epoch,
    Intent, IntentMessage, Multiaddr, NetworkPublicKey, PrimaryInfo, ValidatorSignature,
};
use clap::Parser;
use eyre::Context;
use fastcrypto::traits::{InsecureDefault, Signer};
use reth::node::NodeCommand;
use reth_primitives::{keccak256, Address, ChainSpec, Genesis};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fmt::{Display, Formatter},
    fs,
    path::Path,
    sync::Arc,
};
use tracing::{info, warn};

pub const GENESIS_VALIDATORS_DIR: &'static str = "validators";

/// Return a [NodeCommand] with default args parsed by `clap`.
pub fn execution_args() -> NodeCommand {
    NodeCommand::<()>::try_parse_from(["reth", "--dev", "--chain", &yukon_genesis_string()])
        .expect("clap parse node command")
}

/// Yukon parsed Genesis.
pub fn yukon_genesis() -> Genesis {
    let yaml = yukon_genesis_string();
    serde_json::from_str(&yaml).expect("serde parse valid yukon yaml")
}

/// Yukon chain spec parsed from genesis.
pub fn yukon_chain_spec() -> ChainSpec {
    yukon_genesis().into()
}

/// Yukon chain spec parsed from genesis and wrapped in [Arc].
pub fn yukon_chain_spec_arc() -> Arc<ChainSpec> {
    Arc::new(yukon_chain_spec())
}

/// Yukon genesis string in yaml format.
///
/// Seed "Bob" and [0; 32] seed addresses.
pub fn yukon_genesis_string() -> String {
    yukon_genesis_raw().to_string()
}

/// Static strig for yukon genesis.
///
/// Used by CLI and other methods above.
pub fn yukon_genesis_raw() -> &'static str {
    r#"
{
    "nonce": "0x0",
    "timestamp": "0x6553A8CC",
    "extraData": "0x21",
    "gasLimit": "0x1c9c380",
    "difficulty": "0x0",
    "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "coinbase": "0x0000000000000000000000000000000000000000",
    "alloc": {
        "0x6Be02d1d3665660d22FF9624b7BE0551ee1Ac91b": {
            "balance": "0x4a47e3c12448f4ad000000"
        },
        "0xb14d3c4f5fbfbcfb98af2d330000d49c95b93aa7": {
            "balance": "0x4a47e3c12448f4ad000000"
        }
    },
    "number": "0x0",
    "gasUsed": "0x0",
    "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "config": {
        "ethash": {},
        "chainId": 2600,
        "homesteadBlock": 0,
        "eip150Block": 0,
        "eip155Block": 0,
        "eip158Block": 0,
        "byzantiumBlock": 0,
        "constantinopleBlock": 0,
        "petersburgBlock": 0,
        "istanbulBlock": 0,
        "berlinBlock": 0,
        "londonBlock": 0,
        "terminalTotalDifficulty": 0,
        "terminalTotalDifficultyPassed": true,
        "shanghaiTime": 0
    }
}"#
}

/// The struct for starting a network at genesis.
pub struct NetworkGenesis {
    // /// The committee
    // committee: Committee,
    /// Execution data
    chain: ChainSpec,
    /// Validator signatures
    validators: BTreeMap<BlsPublicKey, ValidatorInfo>,
    // // Validator signatures over checkpoint
    // signatures: BTreeMap<BlsPublicKey, ValidatorSignatureInfo>,
}

impl NetworkGenesis {
    /// Create new version of [NetworkGenesis] using the yukon genesis [ChainSpec].
    pub fn new() -> Self {
        Self { chain: yukon_genesis().into(), validators: Default::default() }
    }

    /// Add validator information to the genesis directory.
    ///
    /// Adding [ValidatorInfo] to the genesis directory allows other
    /// validators to discover peers using VCS (ie - github).
    pub fn add_validator(&mut self, validator: ValidatorInfo) {
        self.validators.insert(validator.public_key().clone(), validator);
    }

    /// Generate a [NetworkGenesis] by reading files in a directory.
    pub fn load_from_path<P>(path: P) -> eyre::Result<Self>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        info!(target: "genesis::ceremony", ?path, "Loading Network Genesis");

        if !path.is_dir() {
            eyre::bail!("path must be a directory");
        }

        // Load validator information
        let mut validators = BTreeMap::new();
        for entry in fs::read_dir(path.join(GENESIS_VALIDATORS_DIR))? {
            let entry = entry?;
            let path = entry.path();

            // Check if it's a file and has the .yaml extension and does not start with '.'
            if path.is_file() &&
                path.file_name().and_then(OsStr::to_str).map_or(true, |s| !s.starts_with('.'))
            {
                // TODO: checking this is probably more trouble than it's worth
                // && path.extension().and_then(OsStr::to_str) == Some("yaml")

                let info_bytes = fs::read(&path)?;
                let validator: ValidatorInfo = serde_yaml::from_slice(&info_bytes)
                    .with_context(|| format!("validator failed to load from {}", path.display()))?;
                validators.insert(validator.bls_public_key.clone(), validator);
            } else {
                warn!("skipping dir: {}\ndirs should not be in validators dir", path.display());
            }
        }

        let network_genesis = Self {
            chain: yukon_genesis().into(),
            validators,
            // signatures,
        };

        Ok(network_genesis)

        // // Load Signatures ? - this seems unnecessary
        // // - validators already include proof-of-possession
        // let mut signatures = BTreeMap::new();
        // for entry in fs::read_dir(path.join(GENESIS_SIGNATURES_DIR))? {
        //     let entry = entry?;
        //     let path = entry.path();

        //     // Check if it's a file and has the .yaml extension and does not start with '.'
        //     if path.is_file()
        //         && path.extension().and_then(OsStr::to_str) == Some("yaml")
        //         && path.file_name().and_then(OsStr::to_str).map_or(true, |s| !s.starts_with('.'))
        // {

        //         info!(target: "genesis::ceremony", "reading validator signatures from {}",
        // path.display());

        //         let signature_bytes = fs::read(path)?;
        //         // TODO: use rlp encode
        //         let sigs: ValidatorSignatureInfo = bcs::from_bytes(&signature_bytes)
        //             .with_context(|| format!("failed to load validator signature info"))?;
        //         signatures.insert(sigs.authority.clone(), sigs);
        //     } else {
        //         warn!("skipping dir: {}\ndirs should not be in signatures", path.display());
        //     }
        // }

        // let unsigned_genesis_file = path.join(GENESIS_BUILDER_UNSIGNED_GENESIS_FILE);
        // if unsigned_genesis_file.exists() {
        //     let unsigned_genesis_bytes = fs::read(unsigned_genesis_file)?;
        //     let loaded_genesis: UnsignedGenesis = bcs::from_bytes(&unsigned_genesis_bytes)?;

        //     // If we have a built genesis, then we must have a token_distribution_schedule
        // present     // as well.
        //     assert!(
        //         builder.token_distribution_schedule.is_some(),
        //         "If a built genesis is present, then there must also be a
        // token-distribution-schedule present"     );

        //     // Verify loaded genesis matches one build from the constituent parts
        //     let built = builder.build_unsigned_genesis_checkpoint();
        //     loaded_genesis.checkpoint_contents.digest(); // cache digest before compare
        //     assert_eq!(
        //         built, loaded_genesis,
        //         "loaded genesis does not match built genesis"
        //     );

        //     // Just to double check that its set after building above
        //     assert!(builder.unsigned_genesis_checkpoint().is_some());
        // }
    }

    /// Write [NetworkGenesis] to path (genesis directory) as individual validator files.
    pub fn write_to_path<P>(self, path: P) -> eyre::Result<()>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        info!(target: "genesis::ceremony", ?path, "Writing Network Genesis to dir");

        fs::create_dir_all(path)?;

        // // Write Signatures?
        // // Are signature necessary?
        // // The validator info already includes a signature over chainspec/genesis
        //
        // let signature_dir = path.join(GENESIS_SIGNATURES_DIR);
        // fs::create_dir_all(&signature_dir)?;
        // for (pubkey, sigs) in self.signatures {
        //     let sig_bytes = bcs::to_bytes(&sigs)?;
        //     // hash validator pubkey
        //     fs::write(signature_dir.join(&file_name), sig_bytes)?;
        // }

        // Write validator infos
        let committee_dir = path.join(GENESIS_VALIDATORS_DIR);
        fs::create_dir_all(&committee_dir)?;

        for (pubkey, validator) in self.validators {
            let validator_info = serde_yaml::to_string(&validator)?;
            let file_name = format!("{}.yaml", keccak256(pubkey)); //.to_string();
            fs::write(committee_dir.join(file_name), validator_info)?;
        }

        // TODO: probably remove this concept
        //
        // if let Some(genesis) = &self.built_genesis {
        //     let genesis_bytes = bcs::to_bytes(&genesis)?;
        //     fs::write(
        //         path.join(GENESIS_BUILDER_UNSIGNED_GENESIS_FILE),
        //         genesis_bytes,
        //     )?;
        // }

        Ok(())
    }

    /// Validate each validator:
    /// - verify proof of possession
    ///
    /// TODO: addition validation?
    ///     - validator name isn't default
    ///     - ???
    pub fn validate(&self) -> eyre::Result<()> {
        for (pubkey, validator) in self.validators.iter() {
            info!(target: "genesis::validate", "verifying validator: {}", pubkey);
            verify_proof_of_possession(&validator.proof_of_possession, pubkey, &self.chain)?;
        }
        info!(target: "genesis::validate", "all validators valid for genesis");
        Ok(())
    }

    /// Create a committee from the validators in [NetworkGenesis].
    pub fn create_committee(&self) -> eyre::Result<Committee> {
        let mut committee_builder = CommitteeBuilder::new(0);
        for (pubkey, validator) in self.validators.iter() {
            committee_builder.add_authority(
                pubkey.clone(),
                1,
                validator.primary_network_address().clone(),
                validator.execution_address,
                validator.primary_network_key().clone(),
                "hostname".to_string(),
            );
        }
        Ok(committee_builder.build())
    }
}

/// information needed for every validator:
/// - name (not strictly necessary but nice to have)
/// - bls public key bytes (not `BlsPublicKey` - need to ensure BlsPublikKey::from_bytes() works)
/// - primary_network_address (Multiaddress)
/// - execution address (Address - not needed for validation, but for suggested fee recipient)
/// - network key (only one for now? - worker and primary share?)
/// - hostname
/// - worker index (HashMap<WorkerId, WorkerInfo>) - create worker cache
/// - p2p address (put in now for execution clients later?)
#[derive(Serialize, Deserialize, PartialEq, Clone, Debug)]
pub struct ValidatorInfo {
    /// The name for the validator. The default value
    /// is the hashed value of the validator's
    /// execution address. The operator can overwrite
    /// this value since it is not used when writing to file.
    ///
    /// Keccak256(Address)
    pub name: String,
    /// [BlsPublicKey] to verify signature.
    pub bls_public_key: BlsPublicKey,
    /// Information for this validator's primary,
    /// including worker details.
    pub primary_info: PrimaryInfo,
    /// The address for suggested fee recipient.
    ///
    /// Validator rewards are sent to this address.
    pub execution_address: Address,
    /// Proof
    pub proof_of_possession: BlsSignature,
    // TODO: remove these for now since they don't seem critical

    // /// Hostname for the node.
    // hostname: String,
    // /// Peer address for execution clients?
    // p2p_address: Multiaddr,
}

impl ValidatorInfo {
    /// Create a new instance of [ValidatorInfo] using the provided data.
    pub fn new(
        name: String,
        bls_public_key: BlsPublicKey,
        primary_info: PrimaryInfo,
        execution_address: Address,
        proof_of_possession: BlsSignature,
    ) -> Self {
        Self { name, bls_public_key, primary_info, execution_address, proof_of_possession }
    }

    /// Return public key bytes.
    pub fn public_key(&self) -> &BlsPublicKey {
        &self.bls_public_key
    }

    /// Return the primary's network address.
    pub fn primary_network_key(&self) -> &NetworkPublicKey {
        &self.primary_info.network_key
    }

    /// Return the primary's network address.
    pub fn primary_network_address(&self) -> &Multiaddr {
        &self.primary_info.network_address
    }
}

impl Default for ValidatorInfo {
    fn default() -> Self {
        Self {
            name: "DEFAULT".to_string(),
            bls_public_key: BlsPublicKey::insecure_default(),
            primary_info: Default::default(),
            execution_address: Address::ZERO,
            proof_of_possession: BlsSignature::default(),
        }
    }
}
#[derive(Clone, Debug, Eq, Serialize, Deserialize)]
pub struct ValidatorSignatureInfo {
    pub epoch: Epoch,
    pub authority: BlsPublicKey,
    pub signature: BlsSignature,
}

impl ValidatorSignatureInfo {
    pub fn new<T>(
        epoch: Epoch,
        value: &T,
        intent: Intent,
        authority: BlsPublicKey,
        secret: &dyn Signer<BlsSignature>,
    ) -> Self
    where
        T: Serialize,
    {
        Self {
            epoch,
            authority,
            signature: BlsSignature::new_secure(&IntentMessage::new(intent, value), secret),
        }
    }
}

impl Display for ValidatorSignatureInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AuthoritySignatureInfo {{ epoch: {:?}, authority: {} }}",
            self.epoch, self.authority,
        )
    }
}

impl PartialEq for ValidatorSignatureInfo {
    fn eq(&self, other: &Self) -> bool {
        // Do not compare the signature. It's possible to have multiple
        // valid signatures for the same epoch and authority.
        self.epoch == other.epoch && self.authority == other.authority
    }
}

#[cfg(test)]
mod tests {
    use super::NetworkGenesis;
    use crate::{
        generate_proof_of_possession, yukon_chain_spec, BlsKeypair, Multiaddr, NetworkKeypair,
        PrimaryInfo, ValidatorInfo, WorkerIndex, WorkerInfo,
    };
    use fastcrypto::traits::KeyPair;
    use rand::{rngs::StdRng, SeedableRng};
    use reth_primitives::Address;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    #[test]
    fn test_write_and_read_network_genesis() {
        let mut network_genesis = NetworkGenesis::new();
        let path = tempdir().expect("tempdir created").into_path();
        // create keys and information for validator
        let bls_keypair = BlsKeypair::generate(&mut StdRng::from_seed([0; 32]));
        let network_keypair = NetworkKeypair::generate(&mut StdRng::from_seed([0; 32]));
        let address = Address::from_raw_public_key(&[0; 64]);
        let proof_of_possession =
            generate_proof_of_possession(&bls_keypair, &yukon_chain_spec()).unwrap();
        let primary_network_address = Multiaddr::empty();
        let worker_info = WorkerInfo::default();
        let worker_index = WorkerIndex(BTreeMap::from([(0, worker_info)]));
        let primary_info = PrimaryInfo::new(
            network_keypair.public().clone(),
            primary_network_address,
            network_keypair.public().clone(),
            worker_index,
        );
        let name = "validator1".to_string();
        // create validator
        let validator = ValidatorInfo::new(
            name,
            bls_keypair.public().clone(),
            primary_info,
            address,
            proof_of_possession,
        );
        // add validator
        network_genesis.add_validator(validator.clone());
        // save to file
        network_genesis.write_to_path(&path).unwrap();
        // load network genesis
        let loaded_network_genesis =
            NetworkGenesis::load_from_path(&path).expect("unable to load network genesis");
        let loaded_validator =
            loaded_network_genesis.validators.get(validator.public_key()).unwrap();
        assert_eq!(&validator, loaded_validator);
    }

    #[test]
    fn test_validate_genesis() {
        let mut network_genesis = NetworkGenesis::new();
        // create keys and information for validators
        for v in 0..4 {
            let bls_keypair = BlsKeypair::generate(&mut StdRng::from_seed([0; 32]));
            let network_keypair = NetworkKeypair::generate(&mut StdRng::from_seed([0; 32]));
            let address = Address::from_raw_public_key(&[0; 64]);
            let proof_of_possession =
                generate_proof_of_possession(&bls_keypair, &yukon_chain_spec()).unwrap();
            let primary_network_address = Multiaddr::empty();
            let worker_info = WorkerInfo::default();
            let worker_index = WorkerIndex(BTreeMap::from([(0, worker_info)]));
            let primary_info = PrimaryInfo::new(
                network_keypair.public().clone(),
                primary_network_address,
                network_keypair.public().clone(),
                worker_index,
            );
            let name = format!("validator-{}", v);
            // create validator
            let validator = ValidatorInfo::new(
                name,
                bls_keypair.public().clone(),
                primary_info,
                address,
                proof_of_possession,
            );
            // add validator
            network_genesis.add_validator(validator.clone());
        }
        // validate
        assert!(network_genesis.validate().is_ok())
    }

    #[test]
    fn test_validate_genesis_fails() {
        // this uses `yukon_genesis`
        let mut network_genesis = NetworkGenesis::new();
        // create keys and information for validators
        for v in 0..4 {
            let bls_keypair = BlsKeypair::generate(&mut StdRng::from_seed([0; 32]));
            let network_keypair = NetworkKeypair::generate(&mut StdRng::from_seed([0; 32]));
            let address = Address::from_raw_public_key(&[0; 64]);

            // create wrong chain spec
            let mut wrong_chain = yukon_chain_spec();
            wrong_chain.genesis.timestamp = 0;

            // generate proof with wrong chain spec
            let proof_of_possession =
                generate_proof_of_possession(&bls_keypair, &wrong_chain).unwrap();
            let primary_network_address = Multiaddr::empty();
            let worker_info = WorkerInfo::default();
            let worker_index = WorkerIndex(BTreeMap::from([(0, worker_info)]));
            let primary_info = PrimaryInfo::new(
                network_keypair.public().clone(),
                primary_network_address,
                network_keypair.public().clone(),
                worker_index,
            );
            let name = format!("validator-{}", v);
            // create validator
            let validator = ValidatorInfo::new(
                name,
                bls_keypair.public().clone(),
                primary_info,
                address,
                proof_of_possession,
            );
            // add validator
            network_genesis.add_validator(validator.clone());
        }
        // validate should fail
        assert!(network_genesis.validate().is_err(), "proof of possession should fail")
    }
}
