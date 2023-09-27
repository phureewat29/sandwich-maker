use anvil::eth::util::get_precompiles_for;
use anyhow::{anyhow, Result};
use cfmms::pool::Pool::{UniswapV2, UniswapV3};
use cfmms::pool::UniswapV2Pool;
use ethers::abi::{self, parse_abi, Address, ParamType};
use ethers::prelude::BaseContract;
use ethers::types::{Bytes, U256};
use foundry_evm::executor::TxEnv;
use foundry_evm::executor::{
    fork::SharedBackend, inspector::AccessListTracer, ExecutionResult, Output, TransactTo,
};
use foundry_evm::revm::{
    db::CacheDB,
    primitives::{Address as rAddress, U256 as rU256},
    EVM,
};

use crate::constants::{GET_RESERVES_SIG, SUGAR_DADDY, WETH_ADDRESS};
use crate::helpers::access_list_to_revm;
use crate::simulator::setup_block_state;
use crate::tx_utils::huff_sandwich_interface::common::five_byte_encoder::FiveByteMetaData;
use crate::tx_utils::huff_sandwich_interface::{
    common::weth_encoder::WethEncoder,
    v2::{v2_create_backrun_payload, v2_create_frontrun_payload},
    v3::{v3_create_backrun_payload, v3_create_frontrun_payload},
};
use crate::types::{BlockInfo, RawIngredients, SandwichRecipe};

use super::salmonella_inspector::{IsSandwichSafu, SalmonellaInspectoooor};

/// finds if sandwich is profitable + salmonella free
pub fn create_recipe(
    ingredients: &RawIngredients,
    next_block: &BlockInfo,
    optimal_in: U256,
    sandwich_start_bal: U256,
    searcher: Address,
    sandwich_address: Address,
    shared_backend: SharedBackend,
) -> Result<SandwichRecipe> {
    #[allow(unused_mut)]
    let mut fork_db = CacheDB::new(shared_backend);

    #[cfg(feature = "debug")]
    {
        inject_huff_sandwich(
            &mut fork_db,
            sandwich_address.0.into(),
            searcher.0.into(),
            sandwich_start_bal,
        );
    }
    let mut evm = EVM::new();
    evm.database(fork_db);
    setup_block_state(&mut evm, &next_block);

    // *´:°•.°+.*•´.*:˚.°*.˚•´.°:°•.°•.*•´.*:˚.°*.˚•´.°:°•.°+.*•´.*:*/
    // *                    FRONTRUN TRANSACTION                    */
    // *.•°:°.´+˚.*°.˚:*.´•*.+°.•°:´*.´•*.•°.•°:°.´:•˚°.*°.˚:*.´+°.•*/
    // encode frontrun_in before passing to sandwich contract
    let frontrun_in = WethEncoder::decode(WethEncoder::encode(optimal_in));

    // caluclate frontrun_out using encoded frontrun_in
    let frontrun_out = match ingredients.get_target_pool() {
        UniswapV2(p) => {
            evm.env.tx.gas_price = next_block.base_fee_per_gas.into();
            evm.env.tx.gas_limit = 700000;
            evm.env.tx.value = rU256::ZERO;
            v2_get_amount_out(frontrun_in, p, true, &mut evm)?
        }
        UniswapV3(_) => U256::zero(),
    };

    // create tx.data and tx.value for frontrun_in
    let (frontrun_data, frontrun_value) = match ingredients.get_target_pool() {
        UniswapV2(p) => v2_create_frontrun_payload(
            p,
            ingredients.get_intermediary_token(),
            frontrun_in,
            frontrun_out,
        ),
        UniswapV3(p) => v3_create_frontrun_payload(
            p,
            ingredients.get_intermediary_token(),
            frontrun_in.as_u128().into(),
        ),
    };

    // setup evm for frontrun transaction
    let mut frontrun_tx_env = TxEnv {
        caller: searcher.0.into(),
        gas_limit: 700000,
        gas_price: next_block.base_fee_per_gas.into(),
        gas_priority_fee: None,
        transact_to: TransactTo::Call(sandwich_address.0.into()),
        value: frontrun_value.into(),
        data: frontrun_data.clone().into(),
        chain_id: None,
        nonce: None,
        access_list: Default::default(),
    };
    evm.env.tx = frontrun_tx_env.clone();

    // get access list
    let mut access_list_inspector = AccessListTracer::new(
        Default::default(),
        searcher,
        sandwich_address,
        get_precompiles_for(evm.env.cfg.spec_id),
    );
    evm.inspect_ref(&mut access_list_inspector)
        .map_err(|e| anyhow!("[EVM ERROR] frontrun: {:?}", (e)))?;
    let frontrun_access_list = access_list_inspector.access_list();

    frontrun_tx_env.access_list = access_list_to_revm(frontrun_access_list);
    evm.env.tx = frontrun_tx_env.clone();

    // run again but now with access list (so that we get accurate gas used)
    // run with a salmonella inspector to flag `suspicious` opcodes
    let mut salmonella_inspector = SalmonellaInspectoooor::new();
    let frontrun_result = match evm.inspect_commit(&mut salmonella_inspector) {
        Ok(result) => result,
        Err(e) => return Err(anyhow!("[huffsandwich: EVM ERROR] frontrun: {:?}", e)),
    };
    match frontrun_result {
        ExecutionResult::Success { .. } => { /* continue operation */ }
        ExecutionResult::Revert { output, .. } => {
            return Err(anyhow!("[huffsandwich: REVERT] frontrun: {:?}", output));
        }
        ExecutionResult::Halt { reason, .. } => {
            return Err(anyhow!("[huffsandwich: HALT] frontrun: {:?}", reason));
        }
    };
    match salmonella_inspector.is_sandwich_safu() {
        IsSandwichSafu::Safu => { /* continue operation */ }
        IsSandwichSafu::NotSafu(not_safu_opcodes) => {
            return Err(anyhow!(
                "[huffsandwich: FrontrunNotSafu] {:?}",
                not_safu_opcodes
            ))
        }
    }

    let frontrun_gas_used = frontrun_result.gas_used();

    // *´:°•.°+.*•´.*:˚.°*.˚•´.°:°•.°•.*•´.*:˚.°*.˚•´.°:°•.°+.*•´.*:*/
    // *                     MEAT TRANSACTION/s                     */
    // *.•°:°.´+˚.*°.˚:*.´•*.+°.•°:´*.´•*.•°.•°:°.´:•˚°.*°.˚:*.´+°.•*/
    let mut is_meat_good = Vec::new();
    for meat in ingredients.get_meats_ref().iter() {
        evm.env.tx.caller = rAddress::from_slice(&meat.from.0);
        evm.env.tx.transact_to =
            TransactTo::Call(rAddress::from_slice(&meat.to.unwrap_or_default().0));
        evm.env.tx.data = meat.input.0.clone();
        evm.env.tx.value = meat.value.into();
        evm.env.tx.chain_id = meat.chain_id.map(|id| id.as_u64());
        //evm.env.tx.nonce = Some(meat.nonce.as_u64());
        evm.env.tx.gas_limit = meat.gas.as_u64();
        match meat.transaction_type {
            Some(ethers::types::U64([0])) => {
                // legacy tx
                evm.env.tx.gas_price = meat.gas_price.unwrap_or_default().into();
            }
            Some(_) => {
                // type 2 tx
                evm.env.tx.gas_priority_fee = meat.max_priority_fee_per_gas.map(|mpf| mpf.into());
                evm.env.tx.gas_price = meat.max_fee_per_gas.unwrap_or_default().into();
            }
            None => {
                // legacy tx
                evm.env.tx.gas_price = meat.gas_price.unwrap().into();
            }
        }

        // keep track of which meat transactions are successful to filter reverted meats at end
        // remove reverted meats because mempool tx/s gas costs are accounted for by fb
        let res = match evm.transact_commit() {
            Ok(result) => result,
            Err(e) => return Err(anyhow!("[huffsandwich: EVM ERROR] meat: {:?}", e)),
        };
        match res.is_success() {
            true => is_meat_good.push(true),
            false => is_meat_good.push(false),
        }
    }

    // *´:°•.°+.*•´.*:˚.°*.˚•´.°:°•.°•.*•´.*:˚.°*.˚•´.°:°•.°+.*•´.*:*/
    // *                    BACKRUN TRANSACTION                     */
    // *.•°:°.´+˚.*°.˚:*.´•*.+°.•°:´*.´•*.•°.•°:°.´:•˚°.*°.˚:*.´+°.•*/
    // encode backrun_in before passing to sandwich contract
    let backrun_token_in = ingredients.get_intermediary_token();
    let backrun_token_out = ingredients.get_start_end_token();

    // keep some dust
    let backrun_in = get_erc20_balance(backrun_token_in, sandwich_address, next_block, &mut evm)?;
    let backrun_in = match ingredients.get_target_pool() {
        UniswapV2(_) => {
            let mut backrun_in_encoded = FiveByteMetaData::encode(backrun_in, 1);
            backrun_in_encoded.decrement_four_bytes();
            backrun_in_encoded.decode()
        }
        UniswapV3(_) => {
            let backrun_in_encoded = FiveByteMetaData::encode(backrun_in, 1);
            backrun_in_encoded.decode()
        }
    };

    // caluclate backrun_out using encoded backrun_in
    let backrun_out = match ingredients.get_target_pool() {
        UniswapV2(p) => {
            let out = v2_get_amount_out(backrun_in, p, false, &mut evm)?;
            out
        }
        UniswapV3(_p) => U256::zero(), // we don't need to know backrun out for v3
    };

    // create tx.data and tx.value for backrun_in
    let (backrun_data, backrun_value) = match ingredients.get_target_pool() {
        UniswapV2(p) => v2_create_backrun_payload(p, backrun_token_in, backrun_in, backrun_out),
        UniswapV3(p) => (
            v3_create_backrun_payload(p, backrun_token_in, backrun_in),
            U256::zero(),
        ),
    };

    // setup evm for backrun transaction
    let mut backrun_tx_env = TxEnv {
        caller: searcher.0.into(),
        gas_limit: 700000,
        gas_price: next_block.base_fee_per_gas.into(),
        gas_priority_fee: None,
        transact_to: TransactTo::Call(sandwich_address.0.into()),
        value: backrun_value.into(),
        data: backrun_data.clone().into(),
        chain_id: None,
        nonce: None,
        access_list: Default::default(),
    };
    evm.env.tx = backrun_tx_env.clone();

    // create access list
    let mut access_list_inspector = AccessListTracer::new(
        Default::default(),
        searcher,
        sandwich_address,
        get_precompiles_for(evm.env.cfg.spec_id),
    );
    evm.inspect_ref(&mut access_list_inspector)
        .map_err(|e| anyhow!("[huffsandwich: EVM ERROR] frontrun: {:?}", e))
        .unwrap();
    let backrun_access_list = access_list_inspector.access_list();
    backrun_tx_env.access_list = access_list_to_revm(backrun_access_list);
    evm.env.tx = backrun_tx_env.clone();

    // run again but now with access list (so that we get accurate gas used)
    // run with a salmonella inspector to flag `suspicious` opcodes
    let mut salmonella_inspector = SalmonellaInspectoooor::new();
    let backrun_result = match evm.inspect_commit(&mut salmonella_inspector) {
        Ok(result) => result,
        Err(e) => return Err(anyhow!("[huffsandwich: EVM ERROR] backrun: {:?}", e)),
    };
    match backrun_result {
        ExecutionResult::Success { .. } => { /* continue */ }
        ExecutionResult::Revert { output, .. } => {
            return Err(anyhow!("[huffsandwich: REVERT] backrun: {:?}", output));
        }
        ExecutionResult::Halt { reason, .. } => {
            return Err(anyhow!("[huffsandwich: HALT] backrun: {:?}", reason))
        }
    };
    match salmonella_inspector.is_sandwich_safu() {
        IsSandwichSafu::Safu => { /* continue operation */ }
        IsSandwichSafu::NotSafu(not_safu_opcodes) => {
            return Err(anyhow!(
                "[huffsandwich: BACKRUN_NOT_SAFU] bad_opcodes->{:?}",
                not_safu_opcodes
            ))
        }
    }

    let backrun_gas_used = backrun_result.gas_used();

    // *´:°•.°+.*•´.*:˚.°*.˚•´.°:°•.°•.*•´.*:˚.°*.˚•´.°:°•.°+.*•´.*:*/
    // *                      GENERATE REPORTS                      */
    // *.•°:°.´+˚.*°.˚:*.´•*.+°.•°:´*.´•*.•°.•°:°.´:•˚°.*°.˚:*.´+°.•*/
    // caluclate revenue from balance change
    let post_sandwich_bal =
        get_erc20_balance(backrun_token_out, sandwich_address, next_block, &mut evm)?;

    let revenue = post_sandwich_bal
        .checked_sub(sandwich_start_bal)
        .unwrap_or_default();

    // filter only passing meat txs
    let good_meats_only = ingredients
        .get_meats_ref()
        .iter()
        .zip(is_meat_good.iter())
        .filter(|&(_, &b)| b)
        .map(|(s, _)| s.to_owned())
        .collect();

    Ok(SandwichRecipe::new(
        frontrun_tx_env,
        frontrun_gas_used,
        good_meats_only,
        backrun_tx_env,
        backrun_gas_used,
        revenue,
        *next_block,
    ))
}

/// Get the balance of a token in an evm (account for tax)
pub fn get_erc20_balance(
    token: Address,
    owner: Address,
    block: &BlockInfo,
    evm: &mut EVM<CacheDB<SharedBackend>>,
) -> Result<U256> {
    let erc20 = BaseContract::from(
        parse_abi(&["function balanceOf(address) external returns (uint)"]).unwrap(),
    );

    evm.env.tx.transact_to = TransactTo::Call(token.0.into());
    evm.env.tx.data = erc20.encode("balanceOf", owner).unwrap().0;
    evm.env.tx.caller = (*SUGAR_DADDY).into(); // spoof addy with a lot of eth
    evm.env.tx.nonce = None;
    evm.env.tx.gas_price = block.base_fee_per_gas.into();
    evm.env.tx.gas_limit = 700000;
    evm.env.tx.value = rU256::ZERO;

    let result = match evm.transact_ref() {
        Ok(result) => result.result,
        Err(e) => {
            return Err(anyhow!("[get_erc20_balance: EVMError] {:?}", e));
        }
    };

    let output: Bytes = match result {
        ExecutionResult::Success { output, .. } => match output {
            Output::Call(o) => o.into(),
            Output::Create(o, _) => o.into(),
        },
        ExecutionResult::Revert { output, .. } => {
            return Err(anyhow!("[get_erc20_balance: Revert] {:?}", output))
        }
        ExecutionResult::Halt { reason, .. } => {
            return Err(anyhow!("[get_erc20_balance: Halt] {:?}", reason))
        }
    };

    match erc20.decode_output("balanceOf", &output) {
        Ok(tokens) => return Ok(tokens),
        Err(e) => return Err(anyhow!("[get_erc20_balance: ABI Error] {:?}", e)),
    }
}

// Find amount out from an amount in using the k=xy formula
// note: reserve values taken from evm
// note: assuming fee is set to 3% for all pools (not case irl)
//
// Arguments:
// * `amount_in`: amount of token in
// * `target_pool`: address of pool
// * `token_in`: address of token in
// * `token_out`: address of token out
// * `evm`: mutable reference to evm used for query
//
// Returns:
// Ok(U256): amount out
// Err(SimulationError): if error during caluclation
pub fn v2_get_amount_out(
    amount_in: U256,
    target_pool: UniswapV2Pool,
    is_frontrun: bool,
    evm: &mut EVM<CacheDB<SharedBackend>>,
) -> Result<U256> {
    // get reserves
    evm.env.tx.transact_to = TransactTo::Call(target_pool.address().0.into());
    evm.env.tx.caller = (*SUGAR_DADDY).0.into(); // spoof weth address for its ether
    evm.env.tx.value = rU256::ZERO;
    evm.env.tx.data = (*GET_RESERVES_SIG).0.clone(); // getReserves()
    evm.env.tx.nonce = None;
    let result = match evm.transact_ref() {
        Ok(result) => result.result,
        Err(e) => return Err(anyhow!("[get_amount_out_evm: EVM ERROR] {:?}", e)),
    };
    let output: Bytes = match result {
        ExecutionResult::Success { output, .. } => match output {
            Output::Call(o) => o.into(),
            Output::Create(o, _) => o.into(),
        },
        ExecutionResult::Revert { output, .. } => {
            return Err(anyhow!("[get_amount_out_evm: EVM REVERTED] {:?}", output))
        }
        ExecutionResult::Halt { reason, .. } => {
            return Err(anyhow!("[get_amount_out_evm: EVM HALT] {:?}", reason))
        }
    };

    let tokens = abi::decode(
        &vec![
            ParamType::Uint(128),
            ParamType::Uint(128),
            ParamType::Uint(32),
        ],
        &output,
    )
    .unwrap();

    let reserves_0 = tokens[0].clone().into_uint().unwrap();
    let reserves_1 = tokens[1].clone().into_uint().unwrap();

    let other_token = [target_pool.token_a, target_pool.token_b]
        .into_iter()
        .find(|&t| t != *WETH_ADDRESS)
        .unwrap();

    let (input_token, output_token) = if is_frontrun {
        // if frontrun we trade WETH -> TOKEN
        (*WETH_ADDRESS, other_token)
    } else {
        // if backrun we trade TOKEN -> WETH
        (other_token, *WETH_ADDRESS)
    };

    let (reserve_in, reserve_out) = match input_token < output_token {
        true => (reserves_0, reserves_1),
        false => (reserves_1, reserves_0),
    };

    let a_in_with_fee: U256 = amount_in * 997;
    let numerator: U256 = a_in_with_fee * reserve_out;
    let denominator: U256 = reserve_in * 1000 + a_in_with_fee;
    let amount_out: U256 = numerator.checked_div(denominator).unwrap_or(U256::zero());

    Ok(amount_out)
}

#[cfg(feature = "debug")]
fn inject_huff_sandwich(
    db: &mut CacheDB<SharedBackend>,
    huff_sandwich_addy: foundry_evm::executor::B160,
    searcher: foundry_evm::executor::B160,
    sandwich_start_bal: U256,
) {
    // compile huff contract
    let git_root = std::str::from_utf8(
        &std::process::Command::new("git")
            .arg("rev-parse")
            .arg("--show-toplevel")
            .output()
            .expect("Failed to execute git command")
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    let mut contract_dir = std::path::PathBuf::from(git_root);
    contract_dir.push("contract/src");

    let output = std::process::Command::new("huffc")
        .arg("--bin-runtime")
        .arg("sandwich.huff")
        .current_dir(contract_dir)
        .output()
        .expect("Failed to compile huff sandwich contract");

    assert!(output.status.success(), "Command execution failed");

    let huff_sandwich_code = std::str::from_utf8(&output.stdout).unwrap();
    let huff_sandwich_code = <Bytes as std::str::FromStr>::from_str(huff_sandwich_code).unwrap();

    //// insert huff sandwich bytecode
    let huff_sandwich_info = foundry_evm::revm::primitives::AccountInfo::new(
        rU256::ZERO,
        0,
        foundry_evm::executor::Bytecode::new_raw(huff_sandwich_code.0),
    );

    db.insert_account_info(huff_sandwich_addy, huff_sandwich_info);

    // insert and fund lilRouter controller (so we can spoof)
    let searcher_info = foundry_evm::revm::primitives::AccountInfo::new(
        crate::simulator::eth_to_wei(200),
        0,
        foundry_evm::executor::Bytecode::default(),
    );
    db.insert_account_info(searcher, searcher_info);

    // fund huff sandwich with 200 weth
    let slot = foundry_evm::revm::primitives::keccak256(&abi::encode(&[
        abi::Token::Address(huff_sandwich_addy.0.into()),
        abi::Token::Uint(U256::from(3)),
    ]));

    db.insert_account_storage(
        (*WETH_ADDRESS).into(),
        slot.into(),
        sandwich_start_bal.into(),
    )
    .unwrap();
}
