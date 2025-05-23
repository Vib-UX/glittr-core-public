use bitcoin::OutPoint;
use collateralized::CollateralizedAssetData;
use database::COLLATERALIZED_CONTRACT_DATA;
use message::MintBurnOption;
use mint_burn_asset::{MintBurnAssetContract, RatioModel, ReturnCollateral};

use super::*;

impl Updater {
    pub async fn burn_return_collateral(
        &mut self,
        mba: &MintBurnAssetContract,
        return_collateral: &ReturnCollateral,
        tx: &Transaction,
        block_tx: &BlockTx,
        contract_id: &BlockTxTuple,
        burn_option: &MintBurnOption,
    ) -> Option<Flaw> {
        let mut out_values: Vec<u128> = vec![];

        let burned_amount = self
            .unallocated_inputs
            .asset_list
            .list
            .remove(&BlockTx::from_tuple(*contract_id).to_string())
            .unwrap_or(0);

        if burned_amount == 0 {
            return Some(Flaw::InsufficientInputAmount);
        }

        if let Some(collateralized) = &mba.mint_mechanism.collateralized {
            match &collateralized.mint_structure {
                mint_burn_asset::MintStructure::Ratio(ratio_type) => {
                    let process_ratio_result = self.validate_and_calculate_ratio_type(
                        &ratio_type,
                        &burned_amount,
                        burn_option,
                        &tx,
                        &block_tx,
                        true,
                    );

                    if let Ok(_out_value) = process_ratio_result {
                        out_values.push(_out_value);
                    } else {
                        return process_ratio_result.err();
                    }
                }
                mint_burn_asset::MintStructure::Proportional(proportional_type) => {
                    match proportional_type.ratio_model {
                        RatioModel::ConstantProduct => {
                            // Get pool data
                            let first_asset_id: BlockTx;
                            let second_asset_id: BlockTx;
                            if let InputAsset::GlittrAsset(asset_id) = collateralized.input_assets[0] {
                                first_asset_id = BlockTx::from_tuple(asset_id)
                            } else {
                                return Some(Flaw::PoolNotFound);
                            }

                            if let InputAsset::GlittrAsset(asset_id) = collateralized.input_assets[1] {
                                second_asset_id = BlockTx::from_tuple(asset_id)
                            } else {
                                return Some(Flaw::PoolNotFound);
                            }

                            let pool_key = BlockTx::from_tuple(*contract_id).to_string();

                            let pool_data: Result<CollateralizedAssetData, DatabaseError> =
                                self.database.lock().await.get(COLLATERALIZED_CONTRACT_DATA, &pool_key);

                            if pool_data.is_err() {
                                return Some(Flaw::PoolNotFound);
                            }

                            let mut pool_data = pool_data.unwrap();

                            let existing_pool_amounts0 =
                                pool_data.amounts.get(&first_asset_id.to_string());
                            let existing_pool_amounts1 =
                                pool_data.amounts.get(&second_asset_id.to_string());

                            if existing_pool_amounts0.is_none() {
                                return Some(Flaw::PoolNotFound);
                            }

                            if existing_pool_amounts1.is_none() {
                                return Some(Flaw::PoolNotFound);
                            }

                            let existing_pool_amounts0 = existing_pool_amounts0.unwrap().clone();
                            let existing_pool_amounts1 = existing_pool_amounts1.unwrap().clone();

                            // Calculate proportion of pool to return
                            let share = burned_amount
                                .saturating_mul(1_000_000) // Scale for precision
                                .saturating_div(pool_data.total_supply);

                            // Calculate return amounts
                            let return_amount0 = existing_pool_amounts0
                                .saturating_mul(share)
                                .saturating_div(1_000_000);
                            let return_amount1 = existing_pool_amounts1
                                .saturating_mul(share)
                                .saturating_div(1_000_000);

                            if return_amount0 == 0 || return_amount1 == 0 {
                                return Some(Flaw::InsufficientOutputAmount);
                            }

                            // Update pool state
                            pool_data
                                .amounts
                                .insert(first_asset_id.to_string(), existing_pool_amounts0.saturating_sub(return_amount0));
                            pool_data
                                .amounts
                                .insert(second_asset_id.to_string(), existing_pool_amounts1.saturating_sub(return_amount1));
                            pool_data.total_supply =
                                pool_data.total_supply.saturating_sub(burned_amount);

                            if !self.is_read_only {
                                self.database
                                    .lock()
                                    .await
                                    .put(COLLATERALIZED_CONTRACT_DATA, &pool_key, pool_data);
                            }

                            out_values.push(return_amount0);
                            out_values.push(return_amount1);
                        },
                        RatioModel::ConstantSum => {
                            // Get pool data
                            let first_asset_id: BlockTx;
                            let second_asset_id: BlockTx;
                            if let InputAsset::GlittrAsset(asset_id) = collateralized.input_assets[0] {
                                first_asset_id = BlockTx::from_tuple(asset_id)
                            } else {
                                return Some(Flaw::PoolNotFound);
                            }

                            if let InputAsset::GlittrAsset(asset_id) = collateralized.input_assets[1] {
                                second_asset_id = BlockTx::from_tuple(asset_id)
                            } else {
                                return Some(Flaw::PoolNotFound);
                            }

                            let pool_key = BlockTx::from_tuple(*contract_id).to_string();
                            let pool_data: Result<CollateralizedAssetData, DatabaseError> =
                                self.database.lock().await.get(COLLATERALIZED_CONTRACT_DATA, &pool_key);

                            if pool_data.is_err() {
                                return Some(Flaw::PoolNotFound);
                            }
                            let mut pool_data = pool_data.unwrap();

                            let existing_pool_amounts0 = pool_data.amounts.get(&first_asset_id.to_string());
                            let existing_pool_amounts1 = pool_data.amounts.get(&second_asset_id.to_string());

                            if existing_pool_amounts0.is_none() || existing_pool_amounts1.is_none() {
                                return Some(Flaw::PoolNotFound);
                            }

                            let existing_pool_amounts0 = existing_pool_amounts0.unwrap().clone();
                            let existing_pool_amounts1 = existing_pool_amounts1.unwrap().clone();

                            // For a constant-sum pool, the withdrawal is linear.
                            // Calculate the share of the pool being redeemed.
                            let share = burned_amount
                                .saturating_mul(1_000_000) // Scale for precision
                                .saturating_div(pool_data.total_supply);

                            // Calculate return amounts proportionally
                            let return_amount0 = existing_pool_amounts0
                                .saturating_mul(share)
                                .saturating_div(1_000_000);
                            let return_amount1 = existing_pool_amounts1
                                .saturating_mul(share)
                                .saturating_div(1_000_000);

                            if return_amount0 == 0 || return_amount1 == 0 {
                                return Some(Flaw::InsufficientOutputAmount);
                            }

                            // Update pool state by subtracting the withdrawn amounts.
                            pool_data.amounts.insert(
                                first_asset_id.to_string(),
                                existing_pool_amounts0.saturating_sub(return_amount0),
                            );
                            pool_data.amounts.insert(
                                second_asset_id.to_string(),
                                existing_pool_amounts1.saturating_sub(return_amount1),
                            );
                            pool_data.total_supply = pool_data.total_supply.saturating_sub(burned_amount);

                            if !self.is_read_only {
                                self.database
                                    .lock()
                                    .await
                                    .put(COLLATERALIZED_CONTRACT_DATA, &pool_key, pool_data);
                            }

                            out_values.push(return_amount0);
                            out_values.push(return_amount1);
                        },
                    }
                }
                mint_burn_asset::MintStructure::Account(_account_type) => {
                    let collateral_account: Option<CollateralAccount> = self
                        .unallocated_inputs
                        .collateral_accounts
                        .collateral_accounts
                        .remove(&BlockTx::from_tuple(*contract_id).to_string());

                    if collateral_account.is_none() {
                        return Some(Flaw::CollateralAccountNotFound);
                    }

                    let mut collateral_account = collateral_account.unwrap();
                    let collateral_account_outpoint: Option<OutPoint> = self
                        .unallocated_inputs
                        .helper_outpoint_collateral_accounts
                        .remove(&collateral_account);

                    if let Some(oracle_message_signed) = &burn_option.oracle_message {
                        if let Some(expected_input_outpoint) =
                            oracle_message_signed.message.input_outpoint
                        {
                            if let Some(oracle_setting) = &return_collateral.oracle_setting {
                                if expected_input_outpoint != collateral_account_outpoint.unwrap() {
                                    return Some(Flaw::OracleMintFailed);
                                }

                                let oracle_validate = self.validate_oracle_message(
                                    oracle_message_signed,
                                    oracle_setting,
                                    block_tx,
                                );

                                if oracle_validate.is_some() {
                                    return oracle_validate;
                                }
                            }

                            // LTV and outstanding always updated by the oracle
                            // LTV is based on off-chain currency (e.g. collateralized * USD)
                            // Outstanding could also include interest
                            if let Some(ltv) = oracle_message_signed.message.ltv {
                                collateral_account.ltv = ltv;
                            } else {
                                return Some(Flaw::LtvMustBeUpdated);
                            }

                            if let Some(outstanding) = &oracle_message_signed.message.outstanding {
                                collateral_account.amount_outstanding = outstanding.0;
                            } else {
                                return Some(Flaw::OutstandingMustBeUpdated);
                            }

                            // verify burned asset
                            if let Some(out_value) = &oracle_message_signed.message.out_value {
                                if burned_amount < out_value.0 {
                                    return Some(Flaw::BurnValueIncorrect);
                                }

                                let burned_remainder = burned_amount - out_value.0;

                                if burned_remainder > 0 {
                                    if let Some(pointer) = burn_option.pointer {
                                        if let Some(flaw) = self.validate_pointer(pointer, tx) {
                                            return Some(flaw);
                                        }

                                        self.allocate_new_asset(
                                            pointer,
                                            &contract_id,
                                            burned_remainder,
                                        )
                                        .await;
                                    } else {
                                        self.unallocated_inputs.asset_list.list.insert(
                                            BlockTx::from_tuple(*contract_id).to_string(),
                                            burned_remainder,
                                        );
                                    }
                                }
                            } else {
                                return Some(Flaw::OutValueNotFound);
                            }
                        }
                    }

                    if let Some(pointer_to_key) = burn_option.pointer_to_key {
                        if let Some(flaw) = self.validate_pointer(pointer_to_key, tx) {
                            return Some(flaw);
                        }

                        self.allocate_new_collateral_accounts(
                            pointer_to_key,
                            &collateral_account,
                            BlockTx::from_tuple(*contract_id).to_string(),
                        )
                        .await;
                    } else {
                        return Some(Flaw::PointerKeyNotFound);
                    }
                }
            }

            // update the mint data

            if let Some(flaw) = self
                .validate_and_update_supply_cap(
                    contract_id,
                    None,
                    burned_amount,
                    false,
                    false,
                    None,
                )
                .await
            {
                return Some(flaw);
            }

            if let Some(pointer) = burn_option.pointer {
                if let Some(flaw) = self.validate_pointer(pointer, tx) {
                    return Some(flaw);
                }

                // allocate return collateral
                for (pos, out_value) in out_values.iter().enumerate() {
                    if let InputAsset::GlittrAsset(asset_id) = collateralized.input_assets[pos] {
                        self.allocate_new_asset(pointer, &asset_id, *out_value)
                            .await;
                    }
                }
            }
        } else {
            return Some(Flaw::InvalidContractType);
        }

        None
    }

    pub async fn burn(
        &mut self,
        tx: &Transaction,
        block_tx: &BlockTx,
        contract_id: &BlockTxTuple,
        burn_option: &MintBurnOption,
        message: Result<OpReturnMessage, Flaw>
    ) -> Option<Flaw> {
        match message {
            Ok(op_return_message) => match op_return_message.contract_creation {
                Some(contract_creation) => match contract_creation.contract_type {
                    ContractType::Moa(_moa) => None,
                    ContractType::Mba(mba) => {

                        if let Some(flaw) = check_live_time(mba.live_time, mba.end_time, contract_id.0, block_tx.block) {
                            return Some(flaw);
                        }

                        if let Some(return_collateral) = &mba.burn_mechanism.return_collateral {
                            return self
                                .burn_return_collateral(
                                    &mba,
                                    return_collateral,
                                    tx,
                                    block_tx,
                                    contract_id,
                                    burn_option,
                                )
                                .await;
                        } else {
                            return Some(Flaw::NotImplemented);
                        }
                    }
                    _ => Some(Flaw::ContractNotMatch),
                },
                None => Some(Flaw::ContractNotMatch),
            },
            Err(flaw) => Some(flaw),
        }
    }
}
