use std::collections::hash_map::{HashMap, Entry};

use bitcoin::util::base58::ToBase58;

use exonum::blockchain::NodeState;
use exonum::storage::List;

use error::Error as ServiceError;
use details::btc::transactions::{AnchoringTx, FundingTx, TxKind};
use blockchain::consensus_storage::AnchoringConfig;
use blockchain::schema::AnchoringSchema;

use super::{AnchoringHandler, LectKind};
use super::error::Error as HandlerError;

#[doc(hidden)]
impl AnchoringHandler {
    pub fn handle_auditing_state(&mut self,
                                 cfg: AnchoringConfig,
                                 state: &NodeState)
                                 -> Result<(), ServiceError> {
        trace!("Auditing state");
        if state.height() % self.node.check_lect_frequency == 0 {
            // Find lect
            let lect = {
                let mut lects = HashMap::new();
                let anchoring_schema = AnchoringSchema::new(state.view());
                let validators_count = cfg.validators.len() as u32;
                for validator_id in 0..validators_count {
                    if let Some(last_lect) = anchoring_schema.lects(validator_id).last()? {
                        // TODO implement hash and eq for transaction
                        match lects.entry(last_lect.0) {
                            Entry::Occupied(mut v) => {
                                *v.get_mut() += 1;
                            }
                            Entry::Vacant(v) => {
                                v.insert(1);
                            }
                        }
                    }
                }

                if let Some((lect, count)) = lects.iter().max_by_key(|&(_, v)| v) {
                    if *count >= ::majority_count(validators_count as u8) {
                        match TxKind::from(lect.clone()) {
                            TxKind::Anchoring(tx) => LectKind::Anchoring(tx),
                            TxKind::FundingTx(tx) => LectKind::Funding(tx),
                            TxKind::Other(tx) => {
                                let e = HandlerError::IncorrectLect {
                                    reason: "Incorrect lect transaction".to_string(),
                                    tx: tx.into(),
                                };
                                return Err(e.into());
                            }
                        }
                    } else {
                        LectKind::None
                    }
                } else {
                    LectKind::None
                }
            };

            let r = match lect {
                LectKind::Funding(tx) => self.check_funding_lect(tx, state),
                LectKind::Anchoring(tx) => self.check_anchoring_lect(tx),
                LectKind::None => {
                    let e = HandlerError::LectNotFound {
                        height: cfg.nearest_anchoring_height(state.height()),
                    };
                    Err(e.into())
                }
            };
            return r;
        }
        Ok(())
    }

    fn check_funding_lect(&self, tx: FundingTx, context: &NodeState) -> Result<(), ServiceError> {
        let cfg = AnchoringSchema::new(context.view())
            .anchoring_config_by_height(0)?;
        let (_, addr) = cfg.redeem_script();
        if tx != cfg.funding_tx {
            let e = HandlerError::IncorrectLect {
                reason: "Initial funding_tx from cfg is different than in lect".to_string(),
                tx: tx.into(),
            };
            return Err(e.into());
        }
        if tx.find_out(&addr).is_none() {
            let e = HandlerError::IncorrectLect {
                reason: format!("Initial funding_tx has no outputs with address={}",
                                addr.to_base58check()),
                tx: tx.into(),
            };
            return Err(e.into());
        }

        // Checks with access to the `bitcoind`
        if let Some(ref client) = self.client {
            if client.get_transaction(&tx.txid())?.is_none() {
                let e = HandlerError::IncorrectLect {
                    reason: "Initial funding_tx not found in the bitcoin blockchain".to_string(),
                    tx: tx.into(),
                };
                return Err(e.into());
            }
        }

        info!("CHECKED_INITIAL_LECT ====== txid={}", tx.txid());
        Ok(())
    }

    fn check_anchoring_lect(&self, tx: AnchoringTx) -> Result<(), ServiceError> {
        // Checks with access to the `bitcoind`
        if let Some(ref client) = self.client {
            if client.get_transaction(&tx.txid())?.is_none() {
                let e = HandlerError::IncorrectLect {
                    reason: "Lect not found in the bitcoin blockchain".to_string(),
                    tx: tx.into(),
                };
                return Err(e.into());
            }
        }

        info!("CHECKED_LECT ====== txid={}", tx.txid());
        Ok(())
    }
}
