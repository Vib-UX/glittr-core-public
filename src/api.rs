use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::Arc,
};

use super::*;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use bitcoin::{consensus::deserialize, OutPoint, Transaction, Txid};
use bitcoincore_rpc::{Auth, Client, RpcApi};
use serde_json::{json, Value};
use store::database::{DatabaseError, MESSAGE_PREFIX, TRANSACTION_TO_BLOCK_TX_PREFIX};
use tower_http::cors::CorsLayer;
use transaction::message::OpReturnMessage;

// TODO: The database lock could possibly slowing down indexing. Add cache or rate limit for the API.
#[derive(Clone)]
pub struct APIState {
    pub database: Arc<Mutex<Database>>,
    pub rpc: Arc<Client>,
}

#[serde_with::skip_serializing_none]
#[derive(Serialize, Deserialize)]
pub struct ContractInfo {
    pub ticker: Option<String>,
    pub supply_cap: Option<U128>,
    pub divisibility: Option<u8>,
    pub total_supply: U128,
    pub r#type: Option<MintType>,
    pub asset: Option<Vec<u8>>,
}

#[derive(Deserialize, Serialize, Clone)]
#[serde(rename_all = "snake_case")]
pub struct CollateralizedSimple {
    pub assets: Vec<InputAssetSimple>,
}

#[serde_with::skip_serializing_none]
#[derive(Serialize, Deserialize)]
pub struct MintType {
    pub preallocated: Option<bool>,
    pub free_mint: Option<bool>,
    pub purchase_or_burn: Option<bool>,
    pub collateralized: Option<CollateralizedSimple>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct InputAssetSimple {
    pub contract_id: BlockTxString,
    pub ticker: Option<String>,
    pub divisibility: u8,
}

#[derive(Deserialize)]
struct QueryOptions {
    show_contract_info: Option<bool>,
}

pub async fn run_api(database: Arc<Mutex<Database>>) -> Result<(), std::io::Error> {
    let rpc = Client::new(
        CONFIG.btc_rpc_url.as_str(),
        Auth::UserPass(
            CONFIG.btc_rpc_username.clone(),
            CONFIG.btc_rpc_password.clone(),
        ),
    )
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "Failed to connect to Bitcoin RPC",
        )
    })?;

    let shared_state = APIState {
        database,
        rpc: Arc::new(rpc),
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/tx/:txid", get(tx_result))
        .route("/blocktx/:block/:tx", get(get_block_tx))
        .route("/blocktx/ticker/:ticker", get(get_block_tx_by_ticker))
        .route("/assets/:txid/:vout", get(get_assets))
        .route("/asset-contract/:block/:tx", get(get_asset_contract))
        .route(
            "/collateralized/:block/:tx",
            get(get_collateralized_contract),
        )
        .route("/validate-tx", post(validate_tx))
        .with_state(shared_state.clone());

    #[cfg(feature = "helper-api")]
    let app = app
        .merge(helper_api::helper_routes())
        .with_state(shared_state);

    let app = app.layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind(CONFIG.api_url.clone()).await;

    match listener {
        Ok(_) => {}
        Err(error) => {
            if error.to_string().contains("Address already in use") {
                log::error!("Address {} already in use", CONFIG.api_url);
            }
            panic!("Error message: {error:?}");
        }
    }

    log::info!("API is listening on {}", CONFIG.api_url);
    axum::serve(listener.unwrap(), app).await
}

async fn tx_result(
    State(state): State<APIState>,
    Path(txid): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let blocktx: Result<BlockTxTuple, DatabaseError> = state
        .database
        .lock()
        .await
        .get(TRANSACTION_TO_BLOCK_TX_PREFIX, txid.as_str());

    let blocktx = if let Ok(blocktx) = blocktx {
        BlockTx {
            block: blocktx.0,
            tx: blocktx.1,
        }
    } else {
        return Err(StatusCode::NOT_FOUND);
    };

    let message: Result<MessageDataOutcome, DatabaseError> = state
        .database
        .lock()
        .await
        .get(MESSAGE_PREFIX, blocktx.to_string().as_str());

    if let Ok(message) = message {
        if message.flaw.is_some() {
            Ok(Json(
                json!({"is_valid": false, "message": message, "block_tx": blocktx.to_string()}),
            ))
        } else {
            Ok(Json(
                json!({"is_valid": true, "message": message, "block_tx": blocktx.to_string()}),
            ))
        }
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn get_block_tx(
    State(state): State<APIState>,
    Path((block, tx)): Path<(u64, u32)>,
) -> Result<Json<Value>, StatusCode> {
    let message: Result<MessageDataOutcome, DatabaseError> = state
        .database
        .lock()
        .await
        .get(MESSAGE_PREFIX, BlockTx { block, tx }.to_string().as_str());

    if let Ok(message) = message {
        Ok(Json(json!({"is_valid": true, "message": message})))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn get_block_tx_by_ticker(
    State(state): State<APIState>,
    Path(ticker): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let updater = Updater::new(state.database.clone(), true).await;
    let block_tx = updater.get_contract_block_tx_by_ticker(ticker).await;

    if let Ok(block_tx) = block_tx {
        let message: Result<MessageDataOutcome, DatabaseError> = state.database.lock().await.get(
            MESSAGE_PREFIX,
            BlockTx {
                block: block_tx.0,
                tx: block_tx.1,
            }
            .to_string()
            .as_str(),
        );

        if let Ok(message) = message {
            Ok(Json(json!({"is_valid": true, "message": message})))
        } else {
            Err(StatusCode::NOT_FOUND)
        }
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn get_assets(
    State(state): State<APIState>,
    Path((txid, vout)): Path<(String, u32)>,
    options: Query<QueryOptions>,
) -> Result<Json<Value>, StatusCode> {
    let updater = Updater::new(state.database, true).await;
    let outpoint = OutPoint {
        txid: Txid::from_str(txid.as_str()).unwrap(),
        vout,
    };

    let mut asset_list: AssetList = AssetList {
        list: HashMap::new(),
    };
    let mut state_keys: StateKeys = StateKeys {
        contract_ids: HashSet::new(),
    };
    let mut contract_infos = HashMap::new();

    match updater.get_asset_list(&outpoint).await {
        Ok(asset_list_) => {
            asset_list = asset_list_;
            if options.show_contract_info == Some(true) {
                for contract_id in asset_list.list.keys() {
                    let block_tx = BlockTx::from_str(contract_id).unwrap();
                    let contract_info = updater
                        .get_contract_info_by_block_tx(block_tx.to_tuple())
                        .await
                        .unwrap();
                    contract_infos.insert(contract_id.clone(), contract_info);
                }
            }
        }
        Err(_) => (),
    };

    match updater.get_state_keys(&outpoint).await {
        Ok(state_keys_) => {
            state_keys = state_keys_;
            if options.show_contract_info == Some(true) {
                for contract_id in state_keys.contract_ids.iter() {
                    let block_tx = BlockTx::from_tuple(contract_id.clone());
                    let contract_info = updater
                        .get_contract_info_by_block_tx(block_tx.to_tuple())
                        .await
                        .unwrap();
                    contract_infos.insert(block_tx.to_string(), contract_info);
                }
            }
        }
        Err(_) => (),
    }

    match updater.get_collateral_accounts(&outpoint).await {
        Ok(collateral_account) => {
            for contract_id_string in collateral_account.collateral_accounts.keys() {
                state_keys
                    .contract_ids
                    .insert(BlockTx::from_str(&contract_id_string).unwrap().to_tuple());
                if options.show_contract_info == Some(true) {
                    let block_tx = BlockTx::from_str(&contract_id_string).unwrap();
                    let contract_info = updater
                        .get_contract_info_by_block_tx(block_tx.to_tuple())
                        .await
                        .unwrap();
                    contract_infos.insert(block_tx.to_string(), contract_info);
                }
            }
        }
        Err(_) => (),
    }

    let mut result = json!({"assets": asset_list});

    if state_keys.contract_ids.len() > 0 {
        result["state_keys"] = json!(state_keys
            .contract_ids
            .iter()
            .map(|contract_id| BlockTx::from_tuple(contract_id.clone()).to_string())
            .collect::<Vec<String>>());
    }

    if contract_infos.len() > 0 {
        result["contract_info"] = json!(contract_infos);
    }
    return Ok(Json(result));
}

async fn get_asset_contract(
    State(state): State<APIState>,
    Path((block, tx)): Path<(u64, u32)>,
) -> Result<Json<Value>, StatusCode> {
    let updater = Updater::new(state.database, true).await;
    if let Ok(asset_contract_data) = updater.get_asset_contract_data(&(block, tx)).await {
        let contract_info = updater
            .get_contract_info_by_block_tx((block, tx))
            .await
            .unwrap();

        if let Ok(collateralized_contract_data) =
            updater.get_collateralized_contract_data(&(block, tx)).await
        {
            let contract_info = updater
                .get_contract_info_by_block_tx((block, tx))
                .await
                .unwrap();

            Ok(Json(
                json!({ "asset": asset_contract_data, "collateralized": collateralized_contract_data, "contract_info": contract_info }),
            ))
        } else {
            Ok(Json(
                json!({ "asset": asset_contract_data, "contract_info": contract_info }),
            ))
        }
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn get_collateralized_contract(
    State(state): State<APIState>,
    Path((block, tx)): Path<(u64, u32)>,
) -> Result<Json<Value>, StatusCode> {
    let updater = Updater::new(state.database, true).await;
    if let Ok(collateralized_contract_data) =
        updater.get_collateralized_contract_data(&(block, tx)).await
    {
        let contract_info = updater
            .get_contract_info_by_block_tx((block, tx))
            .await
            .unwrap();

        Ok(Json(
            json!({ "assets": collateralized_contract_data, "contract_info": contract_info }),
        ))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn validate_tx(
    State(state): State<APIState>,
    body: String,
) -> Result<Json<Value>, StatusCode> {
    let tx_bytes = if let Ok(tx_bytes) = hex::decode(body) {
        tx_bytes
    } else {
        return Ok(Json(
            json!({"is_valid": false, "msg": "Cannot decode hex string"}),
        ));
    };

    let tx: Transaction = if let Ok(tx) = deserialize(&tx_bytes) {
        tx
    } else {
        return Ok(Json(
            json!({"is_valid": false, "msg": "Cannot deserialize to bitcoin transaction"}),
        ));
    };

    if let Ok(op_return_message) = OpReturnMessage::parse_tx(&tx) {
        // Get current block height for validation
        let current_block_tip = state.rpc.get_block_count().unwrap();
        let mut temp_updater = Updater::new(Arc::clone(&state.database), true).await;
        let _ = temp_updater.unallocate_inputs(&tx).await;
        if let Ok(outcome) = temp_updater
            .index(current_block_tip, 1, &tx, Ok(op_return_message))
            .await
        {
            if let Some(flaw) = outcome.flaw {
                Ok(Json(json!({"is_valid": false, "msg": flaw})))
            } else {
                Ok(Json(json!({"is_valid": true})))
            }
        } else {
            Ok(Json(json!({"is_valid": false, "msg": "Error"})))
        }
    } else {
        Ok(Json(
            json!({"is_valid": false, "msg": "Not a valid Glittr message"}),
        ))
    }
}

async fn health() -> &'static str {
    "OK"
}
