use std::str::FromStr;
use message::MintOption;

use super::*;

impl Updater {
    async fn get_free_mint_data(
        &self,
        contract_id: &BlockTxTuple,
    ) -> Result<AssetContractDataFreeMint, Flaw> {
        let data = match self.get_asset_contract_data(contract_id).await {
            Ok(data) => data,
            Err(flaw) => match flaw {
                Flaw::AssetContractDataNotFound => return Ok(AssetContractDataFreeMint::default()),
                _ => return Err(flaw),
            },
        };

        match data {
            AssetContractData::FreeMint(free_mint) => Ok(free_mint),
            // TODO: add error when enum type doesn't match
        }
    }

    async fn update_asset_list_for_mint(
        &self,
        contract_id: &BlockTxTuple,
        outpoint: &Outpoint,
        amount: u32,
    ) -> Option<Flaw> {
        let mut asset_list = match self.get_asset_list(outpoint).await {
            Ok(data) => data,
            Err(flaw) => return Some(flaw),
        };
        let block_tx = BlockTx::from_tuple(*contract_id);
        let previous_amount = asset_list.list.get(&block_tx.to_str()).unwrap_or(&0);

        asset_list
            .list
            .insert(block_tx.to_str(), previous_amount.saturating_add(amount));
        self.set_asset_list(outpoint, &asset_list).await;

        None
    }

    async fn update_asset_contract_data_for_free_mint(
        &self,
        contract_id: &BlockTxTuple,
        free_mint_data: &AssetContractDataFreeMint,
    ) {
        self.set_asset_contract_data(
            &contract_id,
            &AssetContractData::FreeMint(free_mint_data.clone()),
        )
        .await
    }

    async fn mint_free_mint(
        &self,
        asset: AssetContractFreeMint,
        tx: &Transaction,
        block_tx: &BlockTx,
        contract_id: &BlockTxTuple,
        mint_option: &MintOption,
    ) -> Option<Flaw> {
        // check livetime
        if asset.live_time > block_tx.block {
            return Some(Flaw::LiveTimeNotReached);
        }

        let mut free_mint_data = match self.get_free_mint_data(contract_id).await {
            Ok(data) => data,
            Err(flaw) => return Some(flaw),
        };

        // check the supply
        if let Some(supply_cap) = asset.supply_cap {
            let next_supply = free_mint_data
                .minted
                .saturating_mul(asset.amount_per_mint)
                .saturating_add(asset.amount_per_mint);

            if next_supply > supply_cap {
                return Some(Flaw::SupplyCapExceeded);
            }
        }
        free_mint_data.minted = free_mint_data.minted.saturating_add(1);

        // check pointer overflow
        if mint_option.pointer >= tx.output.len() as u32 {
            return Some(Flaw::PointerOverflow);
        }
        let outpoint = Outpoint {
            txid: tx.compute_txid().to_string(),
            vout: mint_option.pointer,
        };

        // set the outpoint
        let flaw = self
            .update_asset_list_for_mint(contract_id, &outpoint, asset.amount_per_mint)
            .await;
        if flaw.is_some() {
            return flaw;
        }

        // update the mint data
        self.update_asset_contract_data_for_free_mint(contract_id, &free_mint_data)
            .await;

        None
    }

    pub async fn mint_purchase_burn_swap(
        &self,
        pbs: AssetContractPurchaseBurnSwap,
        tx: &Transaction,
        contract_id: &BlockTxTuple,
        mint_option: MintOption,
    ) -> Option<Flaw> {
        // TODO: All glittr asset transfer logic here is temporary, MUST change when we have transfer logic implemented
        let mut total_in_value_glittr_asset: u32 = 0;
        let mut total_received_value: u128 = 0;

        let mut out_value: u128 = 0;
        let mut vout: Option<u32> = None;

        // VALIDATE INPUT
        match pbs.input_asset {
            InputAsset::GlittrAsset(asset_contract_id) => {
                for txin in tx.input.iter() {
                    if let Some(asset_list) = self.get_asset_list(&Outpoint {
                        txid: txin.previous_output.txid.to_string(),
                        vout: txin.previous_output.vout,
                    }).await.ok() {
                        let block_tx = BlockTx::from_tuple(asset_contract_id);
                        let amount = asset_list.list.get(&block_tx.to_str()).unwrap_or(&0);
                        total_in_value_glittr_asset = *amount;
                    }
                }
            }
            // No need to validate RawBtc
            InputAsset::RawBTC => {}
            // Validated below on oracle mint
            InputAsset::Metaprotocol => {}
        };

        // VALIDATE OUTPUT
        match pbs.transfer_scheme {
            // Ensure that the asset is set to burn
            asset_contract::TransferScheme::Burn => {
                for (pos, output) in tx.output.iter().enumerate() {
                    let mut instructions = output.script_pubkey.instructions();

                    if instructions.next() == Some(Ok(Instruction::Op(opcodes::all::OP_RETURN))) {
                        match pbs.input_asset {
                            InputAsset::RawBTC => {
                                total_received_value = output.value.to_sat() as u128;
                            }
                            InputAsset::GlittrAsset(..) => {
                                // TODO(transfer): simulate transfer function, check if assets are sent here
                                // for now assume transfering glittr asset success
                                total_received_value = total_in_value_glittr_asset as u128;
                            }
                            InputAsset::Metaprotocol => {
                                // TODO(transfer): handle transfer for each and every metaprotocol (ordinal, runes)
                                // need to copy the transfer mechanics for each, make sure the sat is burned
                            }
                        }

                        if mint_option.pointer != pos as u32 {
                            // NOTE: all asset always went to the first non-op-return txout if the pointer is invalid (for burn)
                            vout = Some(mint_option.pointer);
                            break;
                        }
                    } else if vout.is_none() {
                        vout = Some(pos as u32);
                    }
                }
            }
            asset_contract::TransferScheme::Purchase(bitcoin_address) => {
                for output in tx.output.iter() {
                    let address = Address::from_str(bitcoin_address.as_str())
                        .unwrap()
                        .assume_checked();
                    // TODO: bitcoin network from CONFIG
                    let address_from_script = Address::from_script(
                        output.script_pubkey.as_script(),
                        bitcoin::Network::Regtest,
                    );

                    if let Ok(address_from_script) = address_from_script {
                        if address == address_from_script {
                            match pbs.input_asset {
                                InputAsset::RawBTC => {
                                    total_received_value = output.value.to_sat() as u128;
                                }
                                InputAsset::GlittrAsset(..) => {
                                    // TODO(transfer): simulate transfer function, check if assets are sent here
                                    // for now assume transfering glittr asset success
                                    total_received_value = total_in_value_glittr_asset as u128;
                                }
                                InputAsset::Metaprotocol => {
                                    // TODO(transfer): handle transfer for each and every metaprotocol (ordinal, runes)
                                    // need to copy the transfer mechanics for each, make sure the sat is burned
                                }
                            }
                        }
                    }
                }
                vout = Some(mint_option.pointer);
            }
        }

        // VALIDATE OUT_VALUE
        if let asset_contract::TransferRatioType::Fixed { ratio } = pbs.transfer_ratio_type {
            out_value = (total_received_value * ratio.0) / ratio.1;
        } else if let asset_contract::TransferRatioType::Oracle { pubkey, setting } =
            pbs.transfer_ratio_type.clone()
        {
            let verified = if let Some(oracle_message_signed) = mint_option.oracle_message {
                if setting.asset_id == oracle_message_signed.message.asset_id {
                    let pubkey: XOnlyPublicKey =
                        XOnlyPublicKey::from_slice(pubkey.as_slice()).unwrap();

                    if let Ok(signature) = Signature::from_slice(&oracle_message_signed.signature) {
                        let secp = Secp256k1::new();

                        let msg = Message::from_digest_slice(
                            sha256::Hash::hash(
                                serde_json::to_string(&oracle_message_signed.message)
                                    .unwrap()
                                    .as_bytes(),
                            )
                            .as_byte_array(),
                        )
                        .unwrap();

                        out_value = oracle_message_signed.message.out_value;

                        let mut input_found = false;
                        for txin in tx.input.iter() {
                            if txin.previous_output == oracle_message_signed.message.input_outpoint
                            {
                                input_found = true;
                            }
                        }

                        let below_min_in_value =
                            total_received_value < oracle_message_signed.message.min_in_value;

                        !below_min_in_value
                            && input_found
                            && pubkey.verify(&secp, &msg, &signature).is_ok()
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            if !verified {
                return Some(Flaw::OracleMintFailed);
            }
        }

        if vout.unwrap() > tx.output.len() as u32 {
            return Some(Flaw::PointerOverflow);
        }

        let outpoint = Outpoint {
            txid: tx.compute_txid().to_string(),
            vout: vout.unwrap(),
        };

        // set the outpoint
        let flaw = self
            .update_asset_list_for_mint(contract_id, &outpoint, out_value as u32)
            .await;
        if flaw.is_some() {
            return flaw;
        }

        // Ok(MintResult {
        //     out_value,
        //     txout: vout.unwrap(),
        // })
        None
    }

    pub async fn mint(
        &mut self,
        tx: &Transaction,
        block_tx: &BlockTx,
        contract_id: &BlockTxTuple,
        mint_option: &MintOption,
    ) -> Option<Flaw> {
        let message = self.get_message(&contract_id).await;
        match message {
            Ok(op_return_message) => match op_return_message.tx_type {
                TxType::ContractCreation { contract_type } => match contract_type {
                    message::ContractType::Asset(asset) => match asset {
                        AssetContract::FreeMint(free_mint) => {
                            self.mint_free_mint(free_mint, tx, block_tx, contract_id, mint_option)
                                .await
                        }
                        AssetContract::PurchaseBurnSwap(purchase_burn_swap) => self
                            .mint_purchase_burn_swap(
                                purchase_burn_swap,
                                tx,
                                contract_id,
                                mint_option.clone(),
                            )
                            .await,
                        AssetContract::Preallocated { .. } => Some(Flaw::NotImplemented),
                    },
                },
                _ => Some(Flaw::ContractNotMatch),
            },
            Err(flaw) => Some(flaw),
        }
    }
}
