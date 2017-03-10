use std::collections::BTreeMap;

use serde_json;

use bitcoin::util::base58::ToBase58;
use bitcoin::network::constants::Network;

use exonum::storage::StorageValue;
use exonum::crypto::{hash, Hash, HexValue};

use {BITCOIN_NETWORK, AnchoringTx, FundingTx, RedeemScript, AnchoringRpc};
use btc;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AnchoringRpcConfig {
    pub host: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct AnchoringNodeConfig {
    pub rpc: AnchoringRpcConfig,
    pub private_keys: BTreeMap<String, btc::PrivateKey>,
    pub check_lect_frequency: u64,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct AnchoringConfig {
    pub validators: Vec<btc::PublicKey>,
    pub funding_tx: FundingTx,
    pub fee: u64,
    pub frequency: u64,
    pub utxo_confirmations: u64,
}

pub fn generate_anchoring_config(client: &AnchoringRpc,
                                 count: u8,
                                 total_funds: u64)
                                 -> (AnchoringConfig, Vec<AnchoringNodeConfig>) {
    let rpc = AnchoringRpcConfig {
        host: client.url().into(),
        username: client.username().clone(),
        password: client.password().clone(),
    };
    let mut pub_keys = Vec::new();
    let mut node_cfgs = Vec::new();
    let mut priv_keys = Vec::new();

    for idx in 0..count as usize {
        let account = format!("node_{}", idx);
        let (pub_key, priv_key) = client.gen_keypair(&account).unwrap();

        pub_keys.push(pub_key.clone());
        node_cfgs.push(AnchoringNodeConfig::new(rpc.clone()));
        priv_keys.push(priv_key.clone());
    }

    let majority_count = ::majority_count(count);
    let (_, address) =
        client.create_multisig_address(BITCOIN_NETWORK, majority_count, pub_keys.iter())
            .unwrap();
    let tx = FundingTx::create(&client, &address, total_funds).unwrap();

    let genesis_cfg = AnchoringConfig::new(pub_keys, tx);
    for (idx, node_cfg) in node_cfgs.iter_mut().enumerate() {
        node_cfg.private_keys.insert(address.to_base58check(), priv_keys[idx].clone());
    }

    (genesis_cfg, node_cfgs)
}

impl AnchoringConfig {
    pub fn new(validators: Vec<btc::PublicKey>, tx: FundingTx) -> AnchoringConfig {
        AnchoringConfig {
            validators: validators,
            funding_tx: tx,
            fee: 1000,
            frequency: 500,
            utxo_confirmations: 24,
        }
    }

    pub fn network(&self) -> Network {
        BITCOIN_NETWORK
    }

    pub fn redeem_script(&self) -> (btc::RedeemScript, btc::Address) {
        let majority_count = self.majority_count();
        let redeem_script = RedeemScript::from_pubkeys(self.validators.iter(), majority_count)
            .compressed(self.network());
        let addr = btc::Address::from_script(&redeem_script, self.network());
        (redeem_script, addr)
    }

    pub fn nearest_anchoring_height(&self, height: u64) -> u64 {
        height - height % self.frequency as u64
    }

    pub fn majority_count(&self) -> u8 {
        ::majority_count(self.validators.len() as u8)
    }
}

impl StorageValue for AnchoringConfig {
    fn serialize(self) -> Vec<u8> {
        serde_json::to_vec(&self).unwrap()
    }

    fn deserialize(v: Vec<u8>) -> Self {
        serde_json::from_slice(v.as_slice()).unwrap()
    }

    fn hash(&self) -> Hash {
        hash(serde_json::to_vec(&self).unwrap().as_slice())
    }
}

impl AnchoringNodeConfig {
    pub fn new(rpc: AnchoringRpcConfig) -> AnchoringNodeConfig {
        AnchoringNodeConfig {
            rpc: rpc,
            private_keys: BTreeMap::new(),
            check_lect_frequency: 30,
        }
    }
}

implement_serde_hex! {AnchoringTx}
implement_serde_hex! {FundingTx}

#[cfg(test)]
mod tests {
    use serde_json::value::ToJson;
    use serde_json;

    use exonum::crypto::HexValue;
    use AnchoringTx;

    #[test]
    fn anchoring_tx_serde() {
        let hex = "010000000148f4ae90d8c514a739f17dbbd405442171b09f1044183080b23b6557ce82c0990100000000ffffffff0240899500000000001976a914b85133a96a5cadf6cddcfb1d17c79f42c3bbc9dd88ac00000000000000002e6a2c6a2a6a28020000000000000062467691cf583d4fa78b18fafaf9801f505e0ef03baf0603fd4b0cd004cd1e7500000000";
        let tx = AnchoringTx::from_hex(hex).unwrap();
        let json = tx.to_json().to_string();
        let tx2: AnchoringTx = serde_json::from_str(&json).unwrap();

        assert_eq!(tx2, tx);
    }
}