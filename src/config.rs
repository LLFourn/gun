use bdk::{
    bitcoin::Network,
    blockchain::{esplora::EsploraBlockchainConfig, AnyBlockchainConfig},
};
use std::path::PathBuf;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WalletKeys {
    SeedWordsFile,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WalletKind {
    #[serde(rename = "p2wpkh")]
    P2wpkh,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    pub network: Network,
    pub blockchain: AnyBlockchainConfig,
    pub kind: WalletKind,
    pub keys: WalletKeys,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sd_dir: Option<PathBuf>,
}

impl Config {
    pub fn default_config(network: Network) -> Config {
        use Network::*;
        let url = match network {
            Bitcoin => "https://mempool.space/api",
            Testnet => "https://blockstream.info/testnet/api",
            Regtest => "http://localhost:3000",
            Signet => unimplemented!("signet not supported yet!"),
        };

        let blockchain = AnyBlockchainConfig::Esplora(EsploraBlockchainConfig {
            concurrency: Some(10),
            stop_gap: 10,
            ..EsploraBlockchainConfig::new(url.into())
        });

        Config {
            network,
            blockchain,
            kind: WalletKind::P2wpkh,
            keys: WalletKeys::SeedWordsFile,
            sd_dir: None,
        }
    }
}
