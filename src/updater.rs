mod mint;
mod spec;

use std::{collections::HashMap, str::FromStr};

use bitcoin::{
    hashes::{sha256, Hash},
    key::Secp256k1,
    opcodes,
    script::Instruction,
    secp256k1::{schnorr::Signature, Message},
    Address, Transaction, TxOut, XOnlyPublicKey,
};
use database::{
    DatabaseError, ASSET_CONTRACT_DATA_PREFIX, ASSET_LIST_PREFIX, MESSAGE_PREFIX,
    TRANSACTION_TO_BLOCK_TX_PREFIX, VESTING_CONTRACT_DATA_PREFIX,
};
use flaw::Flaw;
use message::{CallType, ContractType, OpReturnMessage, TxTypeTransfer};
use mint_only_asset::{InputAsset, MintOnlyAssetContract, PurchaseBurnSwap, VestingPlan};

use super::*;

#[derive(Deserialize, Serialize, Clone, Default, Debug)]
#[serde(rename_all = "snake_case")]
pub struct AssetContractData {
    pub minted_supply: u128,
    pub burned_supply: u128,
}

#[derive(Deserialize, Serialize, Clone, Default, Debug)]
#[serde(rename_all = "snake_case")]
pub struct AssetList {
    pub list: HashMap<BlockTxString, u128>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub struct MessageDataOutcome {
    pub message: Option<OpReturnMessage>,
    pub flaw: Option<Flaw>,
}
pub struct PBSMintResult {
    pub out_value: u128,
    pub txout: u32,
}

#[derive(Deserialize, Serialize, Clone, Default, Debug)]
#[serde(rename_all = "snake_case")]
pub struct VestingContractData {
    pub claimed_allocations: HashMap<String, u128>,
}

#[derive(Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct SpecContractOwned {
    pub specs: Vec<BlockTxTuple>,
}

#[derive(Default)]
pub struct Allocation {
    asset_list: AssetList,
    spec_owned: SpecContractOwned,
}

pub struct Updater {
    pub database: Arc<Mutex<Database>>,
    is_read_only: bool,

    unallocated_inputs: Allocation,
    allocated_outputs: HashMap<u32, Allocation>,
}

impl Updater {
    pub async fn new(database: Arc<Mutex<Database>>, is_read_only: bool) -> Self {
        Updater {
            database,
            is_read_only,

            unallocated_inputs: Allocation::default(),
            allocated_outputs: HashMap::new(),
        }
    }

    pub async fn unallocate_inputs(&mut self, tx: &Transaction) -> Result<(), Box<dyn Error>> {
        for tx_input in tx.input.iter() {
            let outpoint = &Outpoint {
                txid: tx_input.previous_output.txid.to_string(),
                vout: tx_input.previous_output.vout,
            };

            // set asset_list
            if let Ok(asset_list) = self.get_asset_list(outpoint).await {
                for asset in asset_list.list.iter() {
                    let previous_amount = self
                        .unallocated_inputs
                        .asset_list
                        .list
                        .get(asset.0)
                        .unwrap_or(&0);
                    self.unallocated_inputs.asset_list.list.insert(
                        asset.0.to_string(),
                        previous_amount.saturating_add(*asset.1),
                    );
                }

                // TODO: Implement a backup mechanism to recover when downtime occurs
                self.delete_asset(outpoint).await;
            }

            // set specs
            if let Ok(spec_contract_owned) = self.get_spec_contract_owned(outpoint).await {
                for contract in spec_contract_owned.specs.iter() {
                    self.unallocated_inputs.spec_owned.specs.push(*contract)
                }

                self.delete_spec_contract_owned(outpoint).await
            }
        }

        Ok(())
    }

    pub async fn allocate_new_asset(
        &mut self,
        vout: u32,
        contract_id: &BlockTxTuple,
        amount: u128,
    ) {
        let block_tx = BlockTx::from_tuple(*contract_id);

        let allocation = self.allocated_outputs.entry(vout).or_default();

        let previous_amount = allocation
            .asset_list
            .list
            .entry(block_tx.to_str())
            .or_insert(0);
        *previous_amount = previous_amount.saturating_add(amount);
    }

    pub async fn move_asset_allocation(
        &mut self,
        vout: u32,
        contract_id: &BlockTxTuple,
        max_amount: u128,
    ) {
        let block_tx = BlockTx::from_tuple(*contract_id);
        let Some(allocation) = self
            .unallocated_inputs
            .asset_list
            .list
            .get_mut(&block_tx.to_string())
        else {
            return;
        };

        let amount = max_amount.min(*allocation);
        if amount == 0 {
            return;
        }

        *allocation = allocation.saturating_sub(amount);
        if *allocation == 0 {
            self.unallocated_inputs
                .asset_list
                .list
                .remove(&block_tx.to_string());
        }

        self.allocate_new_asset(vout, contract_id, amount).await;
    }

    pub async fn commit_outputs(&mut self, tx: &Transaction) -> Result<(), Box<dyn Error>> {
        let txid = tx.compute_txid().to_string();

        if let Some(vout) = self.first_non_op_return_index(tx) {
            // asset
            // move unallocated to first non op_return index (fallback)
            let asset_list = self.unallocated_inputs.asset_list.list.clone();
            for asset in asset_list.iter() {
                let block_tx = BlockTx::from_str(asset.0)?;

                self.move_asset_allocation(vout, &block_tx.to_tuple(), *asset.1)
                    .await;
            }

            // specs
            let specs = &self.unallocated_inputs.spec_owned.specs.clone();
            for spec_contract_id in specs.iter() {
                self.allocate_new_spec(vout, spec_contract_id).await;
            };

        } else {
            log::info!("No non op_return index, unallocated outputs are lost");
        }

        for allocation in self.allocated_outputs.iter() {
            let outpoint = &Outpoint {
                txid: txid.clone(),
                vout: *allocation.0,
            };

            self.set_asset_list(outpoint, &allocation.1.asset_list)
                .await;
            self.set_spec_contract_owned(outpoint, &allocation.1.spec_owned)
                .await;
        }

        // reset asset list
        self.unallocated_inputs = Allocation::default();
        self.allocated_outputs = HashMap::new();

        Ok(())
    }

    fn is_op_return_index(&self, output: &TxOut) -> bool {
        let mut instructions = output.script_pubkey.instructions();
        if instructions.next() == Some(Ok(Instruction::Op(opcodes::all::OP_RETURN))) {
            return true;
        }

        false
    }

    fn first_non_op_return_index(&self, tx: &Transaction) -> Option<u32> {
        for (i, output) in tx.output.iter().enumerate() {
            if !self.is_op_return_index(output) {
                return Some(i as u32);
            };
        }

        None
    }

    // run modules here
    pub async fn index(
        &mut self,
        block_height: u64,
        tx_index: u32,
        tx: &Transaction,
        message_result: Result<OpReturnMessage, Flaw>,
    ) -> Result<MessageDataOutcome, Box<dyn Error>> {
        let mut outcome = MessageDataOutcome {
            message: None,
            flaw: None,
        };

        let block_tx = &BlockTx {
            block: block_height,
            tx: tx_index,
        };

        if let Ok(message) = message_result {
            outcome.message = Some(message.clone());
            // NOTE: static validation
            if let Some(flaw) = message.validate() {
                outcome.flaw = Some(flaw)
            }

            if let Some(transfer) = message.transfer {
                if outcome.flaw.is_none() {
                    outcome.flaw = self.transfers(tx, transfer.transfers).await;
                }
            }

            // NOTe: dynamic validation
            if let Some(contract_creation) = message.contract_creation {
                // validate contract creation by spec
                if let Some(spec_contract_id) = contract_creation.spec {
                    if outcome.flaw.is_none() {
                        outcome.flaw = self
                            .validate_contract_by_spec(
                                &spec_contract_id,
                                &contract_creation.contract_type,
                            )
                            .await;
                    }
                };

                match contract_creation.contract_type {
                    ContractType::Asset(asset_contract) => {
                        if let Some(purchase) = asset_contract.mint_mechanism.purchase {
                            if let InputAsset::GlittrAsset(block_tx_tuple) = purchase.input_asset {
                                let message = self.get_message(&block_tx_tuple).await;

                                if let Ok(message) = message {
                                    if message.contract_creation.is_none() && outcome.flaw.is_none()
                                    {
                                        outcome.flaw = Some(Flaw::ReferencingFlawedBlockTx)
                                    }
                                } else if outcome.flaw.is_none() {
                                    outcome.flaw = Some(Flaw::ReferencingFlawedBlockTx);
                                }
                            }
                        }
                    }

                    ContractType::Spec(spec_contract) => {
                        if let Some(contract_id) = spec_contract.block_tx {
                            // update the spec
                            if outcome.flaw.is_none() {
                                outcome.flaw = self.update_spec(&contract_id, &spec_contract).await;
                            }
                        } else {
                            // create the spec
                            if outcome.flaw.is_none() {
                                outcome.flaw = self
                                    .create_spec(block_height, tx_index, tx, &spec_contract)
                                    .await;
                            }
                        }
                    }
                }
            }

            if let Some(contract_call) = message.contract_call {
                match contract_call.call_type {
                    CallType::Mint(mint_option) => {
                        if outcome.flaw.is_none() {
                            outcome.flaw = self
                                .mint(tx, block_tx, &contract_call.contract, &mint_option)
                                .await;
                        }
                    }
                    CallType::Burn => {
                        log::info!("Process call type burn");
                    }
                    CallType::Swap => {
                        log::info!("Process call type swap");
                    }
                }
            }
        } else {
            outcome.flaw = Some(message_result.unwrap_err());
        }

        if !self.is_read_only {
            log::info!(
                "# Outcome {:?}, {:?} at {}",
                outcome.flaw,
                outcome.message,
                block_tx
            );
            self.database.lock().await.put(
                MESSAGE_PREFIX,
                block_tx.to_string().as_str(),
                outcome.clone(),
            );

            self.database.lock().await.put(
                TRANSACTION_TO_BLOCK_TX_PREFIX,
                tx.compute_txid().to_string().as_str(),
                block_tx.to_tuple(),
            );
        }

        Ok(outcome)
    }

    pub async fn transfers(
        &mut self,
        tx: &Transaction,
        transfers: Vec<TxTypeTransfer>,
    ) -> Option<Flaw> {
        let mut overflow_i = Vec::new();

        for (i, transfer) in transfers.iter().enumerate() {
            if transfer.output >= tx.output.len() as u32 {
                overflow_i.push(i as u32);
                continue;
            }
            self.move_asset_allocation(transfer.output, &transfer.asset, transfer.amount.0)
                .await;
        }

        if !overflow_i.is_empty() {
            return Some(Flaw::OutputOverflow(overflow_i));
        }

        None
    }

    async fn delete_asset(&self, outpoint: &Outpoint) {
        if !self.is_read_only {
            self.database
                .lock()
                .await
                .delete(ASSET_LIST_PREFIX, &outpoint.to_string());
        }
    }

    pub async fn get_asset_list(&self, outpoint: &Outpoint) -> Result<AssetList, Flaw> {
        let result: Result<AssetList, DatabaseError> = self
            .database
            .lock()
            .await
            .get(ASSET_LIST_PREFIX, &outpoint.to_string());

        match result {
            Ok(data) => Ok(data),
            Err(DatabaseError::NotFound) => Ok(AssetList::default()),
            Err(DatabaseError::DeserializeFailed) => Err(Flaw::FailedDeserialization),
        }
    }

    async fn set_asset_list(&self, outpoint: &Outpoint, asset_list: &AssetList) {
        if !self.is_read_only {
            self.database
                .lock()
                .await
                .put(ASSET_LIST_PREFIX, &outpoint.to_string(), asset_list);
        }
    }

    async fn get_message(&self, contract_id: &BlockTxTuple) -> Result<OpReturnMessage, Flaw> {
        let contract_key = BlockTx::from_tuple(*contract_id).to_string();
        let outcome: Result<MessageDataOutcome, DatabaseError> = self
            .database
            .lock()
            .await
            .get(MESSAGE_PREFIX, &contract_key);

        match outcome {
            Ok(outcome) => {
                if let Some(flaw) = outcome.flaw {
                    Err(flaw)
                } else {
                    outcome.message.ok_or(Flaw::MessageInvalid)
                }
            }
            Err(DatabaseError::NotFound) => Err(Flaw::ContractNotFound),
            Err(DatabaseError::DeserializeFailed) => Err(Flaw::FailedDeserialization),
        }
    }

    async fn set_message(&self, contract_id: &BlockTxTuple, message: &OpReturnMessage) {
        let outcome = MessageDataOutcome {
            message: Some(message.clone()),
            flaw: None,
        };

        if !self.is_read_only {
            let contract_key = BlockTx::from_tuple(*contract_id).to_string();
            self.database
                .lock()
                .await
                .put(MESSAGE_PREFIX, &contract_key, outcome);
        }
    }

    pub async fn get_asset_contract_data(
        &self,
        contract_id: &BlockTxTuple,
    ) -> Result<AssetContractData, Flaw> {
        let contract_key = BlockTx::from_tuple(*contract_id).to_string();
        let data: Result<AssetContractData, DatabaseError> = self
            .database
            .lock()
            .await
            .get(ASSET_CONTRACT_DATA_PREFIX, &contract_key);

        match data {
            Ok(data) => Ok(data),
            Err(DatabaseError::NotFound) => Ok(AssetContractData::default()),
            Err(DatabaseError::DeserializeFailed) => Err(Flaw::FailedDeserialization),
        }
    }

    async fn set_asset_contract_data(
        &self,
        contract_id: &BlockTxTuple,
        asset_contract_data: &AssetContractData,
    ) {
        if !self.is_read_only {
            let contract_key = BlockTx::from_tuple(*contract_id).to_string();
            self.database.lock().await.put(
                ASSET_CONTRACT_DATA_PREFIX,
                &contract_key,
                asset_contract_data,
            );
        }
    }

    pub async fn get_vesting_contract_data(
        &self,
        contract_id: &BlockTxTuple,
    ) -> Result<VestingContractData, Flaw> {
        let contract_key = BlockTx::from_tuple(*contract_id).to_string();
        let data: Result<VestingContractData, DatabaseError> = self
            .database
            .lock()
            .await
            .get(VESTING_CONTRACT_DATA_PREFIX, &contract_key);

        match data {
            Ok(data) => Ok(data),
            Err(DatabaseError::NotFound) => Ok(VestingContractData::default()),
            Err(DatabaseError::DeserializeFailed) => Err(Flaw::FailedDeserialization),
        }
    }

    async fn set_vesting_contract_data(
        &self,
        contract_id: &BlockTxTuple,
        vesting_contract_data: &VestingContractData,
    ) {
        if !self.is_read_only {
            let contract_key = BlockTx::from_tuple(*contract_id).to_string();
            self.database.lock().await.put(
                VESTING_CONTRACT_DATA_PREFIX,
                &contract_key,
                vesting_contract_data,
            );
        }
    }

    fn validate_pointer(&self, pointer: u32, tx: &Transaction) -> Option<Flaw> {
        if pointer >= tx.output.len() as u32 {
            return Some(Flaw::PointerOverflow);
        }
        if self.is_op_return_index(&tx.output[pointer as usize]) {
            return Some(Flaw::InvalidPointer);
        }
        None
    }
}
