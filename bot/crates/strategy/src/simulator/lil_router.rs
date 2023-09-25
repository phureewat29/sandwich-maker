use anyhow::{anyhow, Result};
use cfmms::pool::Pool::{UniswapV2, UniswapV3};
use ethers::{abi, types::U256};
use foundry_evm::{
    executor::{fork::SharedBackend, Bytecode, ExecutionResult, Output, TransactTo},
    revm::{
        db::CacheDB,
        primitives::{keccak256, AccountInfo, Address as rAddress, U256 as rU256},
        EVM,
    },
};

use crate::{
    constants::{
        LIL_ROUTER_ADDRESS, LIL_ROUTER_CODE, LIL_ROUTER_CONTROLLER, WETH_ADDRESS, WETH_FUND_AMT,
    },
    tx_utils::lil_router_interface::{
        build_swap_v2_data, build_swap_v3_data, decode_swap_v2_result, decode_swap_v3_result,
    },
    types::{BlockInfo, RawIngredients},
};

use super::{eth_to_wei, setup_block_state};

// Juiced implementation of https://research.ijcaonline.org/volume65/number14/pxc3886165.pdf
// splits range in more intervals, search intervals concurrently, compare, repeat till termination
pub async fn find_optimal_input(
    ingredients: &RawIngredients,
    target_block: &BlockInfo,
    weth_inventory: U256,
    shared_backend: SharedBackend,
) -> Result<U256> {
    //
    //            [EXAMPLE WITH 10 BOUND INTERVALS]
    //
    //     (first)              (mid)               (last)
    //        ▼                   ▼                   ▼
    //        +---+---+---+---+---+---+---+---+---+---+
    //        |   |   |   |   |   |   |   |   |   |   |
    //        +---+---+---+---+---+---+---+---+---+---+
    //        ▲   ▲   ▲   ▲   ▲   ▲   ▲   ▲   ▲   ▲   ▲
    //        0   1   2   3   4   5   6   7   8   9   X
    //
    //  * [0, X] = search range
    //  * Find revenue at each interval
    //  * Find index of interval with highest revenue
    //  * Search again with bounds set to adjacent index of highest

    // setup values for search termination
    let base = U256::from(1000000u64);
    let tolerance = U256::from(1u64);

    let mut lower_bound = U256::zero();
    let mut upper_bound = weth_inventory;

    let tolerance = (tolerance * ((upper_bound + lower_bound) / rU256::from(2))) / base;

    // initialize variables for search
    let l_interval_lower = |i: usize, intervals: &Vec<U256>| intervals[i - 1].clone() + 1;
    let r_interval_upper = |i: usize, intervals: &Vec<U256>| {
        intervals[i + 1]
            .clone()
            .checked_sub(1.into())
            .ok_or(anyhow!("r_interval - 1 underflowed"))
    };
    let should_loop_terminate = |lower_bound: U256, upper_bound: U256| -> bool {
        let search_range = match upper_bound.checked_sub(lower_bound) {
            Some(range) => range,
            None => return true,
        };
        // produces negative result
        if lower_bound > upper_bound {
            return true;
        }
        // tolerance condition not met
        if search_range < tolerance {
            return true;
        }
        false
    };
    let mut highest_sandwich_input = U256::zero();
    let number_of_intervals = 15;
    let mut counter = 0;

    // continue search until termination condition is met (no point seraching down to closest wei)
    loop {
        counter += 1;
        if should_loop_terminate(lower_bound, upper_bound) {
            break;
        }

        // split search range into intervals
        let mut intervals = Vec::new();
        for i in 0..=number_of_intervals {
            let diff = upper_bound
                .checked_sub(lower_bound)
                .ok_or(anyhow!("upper_bound - lower_bound resulted in underflow"))?;

            let fraction = diff * i;
            let divisor = U256::from(number_of_intervals);
            let interval = lower_bound + (fraction / divisor);

            intervals.push(interval);
        }

        // calculate revenue at each interval concurrently
        let mut revenues = Vec::new();
        for bound in &intervals {
            let sim = tokio::task::spawn(evaluate_sandwich_revenue(
                *bound,
                target_block.clone(),
                shared_backend.clone(),
                ingredients.clone(),
            ));
            revenues.push(sim);
        }

        let revenues = futures::future::join_all(revenues).await;

        let revenues = revenues
            .into_iter()
            .map(|r| r.unwrap().unwrap_or_default())
            .collect::<Vec<_>>();

        // find interval that produces highest revenue
        let (highest_revenue_index, _highest_revenue) = revenues
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.cmp(&b))
            .unwrap();

        highest_sandwich_input = intervals[highest_revenue_index];

        // enhancement: find better way to increase finding opps incase of all rev=0
        if revenues[highest_revenue_index] == U256::zero() {
            // most likely there is no sandwich possibility
            if counter == 10 {
                return Ok(U256::zero());
            }
            // no revenue found, most likely small optimal so decrease range
            upper_bound = intervals[intervals.len() / 3]
                .checked_sub(1.into())
                .ok_or(anyhow!("intervals[intervals.len()/3] - 1 underflowed"))?;
            continue;
        }

        // if highest revenue is produced at last interval (upper bound stays fixed)
        if highest_revenue_index == intervals.len() - 1 {
            lower_bound = l_interval_lower(highest_revenue_index, &intervals);
            continue;
        }

        // if highest revenue is produced at first interval (lower bound stays fixed)
        if highest_revenue_index == 0 {
            upper_bound = r_interval_upper(highest_revenue_index, &intervals)?;
            continue;
        }

        // set bounds to intervals adjacent to highest revenue index and search again
        lower_bound = l_interval_lower(highest_revenue_index, &intervals);
        upper_bound = r_interval_upper(highest_revenue_index, &intervals)?;
    }

    Ok(highest_sandwich_input)
}

async fn evaluate_sandwich_revenue(
    frontrun_in: U256,
    next_block: BlockInfo,
    shared_backend: SharedBackend,
    ingredients: RawIngredients,
) -> Result<U256> {
    let mut fork_db = CacheDB::new(shared_backend);
    inject_lil_router_code(&mut fork_db);

    let mut evm = EVM::new();
    evm.database(fork_db);
    setup_block_state(&mut evm, &next_block);

    /*´:°•.°+.*•´.*:˚.°*.˚•´.°:°•.°•.*•´.*:˚.°*.˚•´.°:°•.°+.*•´.*:*/
    /*                    FRONTRUN TRANSACTION                    */
    /*.•°:°.´+˚.*°.˚:*.´•*.+°.•°:´*.´•*.•°.•°:°.´:•˚°.*°.˚:*.´+°.•*/
    let frontrun_data = match ingredients.get_target_pool() {
        UniswapV2(pool) => build_swap_v2_data(frontrun_in, pool, true),
        UniswapV3(pool) => build_swap_v3_data(frontrun_in.as_u128().into(), pool, true),
    };

    evm.env.tx.caller = *LIL_ROUTER_CONTROLLER;
    evm.env.tx.transact_to = TransactTo::Call(*LIL_ROUTER_ADDRESS);
    evm.env.tx.data = frontrun_data.0;
    evm.env.tx.gas_limit = 700000;
    evm.env.tx.gas_price = next_block.base_fee_per_gas.into();
    evm.env.tx.value = rU256::ZERO;

    let result = match evm.transact_commit() {
        Ok(result) => result,
        Err(e) => return Err(anyhow!("[lilRouter: EVM ERROR] frontrun: {:?}", e)),
    };
    let output = match result {
        ExecutionResult::Success { output, .. } => match output {
            Output::Call(o) => o,
            Output::Create(o, _) => o,
        },
        ExecutionResult::Revert { output, .. } => {
            return Err(anyhow!("[lilRouter: REVERT] frontrun: {:?}", output))
        }
        ExecutionResult::Halt { reason, .. } => {
            return Err(anyhow!("[lilRouter: HALT] frontrun: {:?}", reason))
        }
    };
    let (_frontrun_out, backrun_in) = match ingredients.get_target_pool() {
        UniswapV2(_) => match decode_swap_v2_result(output.into()) {
            Ok(output) => output,
            Err(e) => {
                return Err(anyhow!(
                    "[lilRouter: FailedToDecodeOutput] frontrun: {:?}",
                    e
                ))
            }
        },
        UniswapV3(_) => match decode_swap_v3_result(output.into()) {
            Ok(output) => output,
            Err(e) => return Err(anyhow!("lilRouter: FailedToDecodeOutput: {:?}", e)),
        },
    };

    /*´:°•.°+.*•´.*:˚.°*.˚•´.°:°•.°•.*•´.*:˚.°*.˚•´.°:°•.°+.*•´.*:*/
    /*                     MEAT TRANSACTION/s                     */
    /*.•°:°.´+˚.*°.˚:*.´•*.+°.•°:´*.´•*.•°.•°:°.´:•˚°.*°.˚:*.´+°.•*/
    for meat in ingredients.get_meats_ref().iter() {
        evm.env.tx.caller = rAddress::from_slice(&meat.from.0);
        evm.env.tx.transact_to =
            TransactTo::Call(rAddress::from_slice(&meat.to.unwrap_or_default().0));
        evm.env.tx.data = meat.input.0.clone();
        evm.env.tx.value = meat.value.into();
        evm.env.tx.chain_id = meat.chain_id.map(|id| id.as_u64());
        // evm.env.tx.nonce = Some(meat.nonce.as_u64()); /** ignore nonce check for now **/
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
                evm.env.tx.gas_price = meat.gas_price.unwrap_or_default().into();
            }
        }

        let _res = evm.transact_commit();
    }

    /*´:°•.°+.*•´.*:˚.°*.˚•´.°:°•.°•.*•´.*:˚.°*.˚•´.°:°•.°+.*•´.*:*/
    /*                    BACKRUN TRANSACTION                     */
    /*.•°:°.´+˚.*°.˚:*.´•*.+°.•°:´*.´•*.•°.•°:°.´:•˚°.*°.˚:*.´+°.•*/
    let backrun_data = match ingredients.get_target_pool() {
        UniswapV2(pool) => build_swap_v2_data(backrun_in, pool, false),
        UniswapV3(pool) => build_swap_v3_data(backrun_in.as_u128().into(), pool, false),
    };

    evm.env.tx.caller = *LIL_ROUTER_CONTROLLER;
    evm.env.tx.transact_to = TransactTo::Call(*LIL_ROUTER_ADDRESS);
    evm.env.tx.data = backrun_data.0;
    evm.env.tx.gas_limit = 700000;
    evm.env.tx.gas_price = next_block.base_fee_per_gas.into();
    evm.env.tx.value = rU256::ZERO;

    let result = match evm.transact_commit() {
        Ok(result) => result,
        Err(e) => return Err(anyhow!("[lilRouter: EVM ERROR] backrun: {:?}", e)),
    };
    let output = match result {
        ExecutionResult::Success { output, .. } => match output {
            Output::Call(o) => o,
            Output::Create(o, _) => o,
        },
        ExecutionResult::Revert { output, .. } => {
            return Err(anyhow!("[lilRouter: REVERT] backrun: {:?}", output))
        }
        ExecutionResult::Halt { reason, .. } => {
            return Err(anyhow!("[lilRouter: HALT] backrun: {:?}", reason))
        }
    };
    let (_backrun_out, post_sandwich_balance) = match ingredients.get_target_pool() {
        UniswapV2(_) => match decode_swap_v2_result(output.into()) {
            Ok(output) => output,
            Err(e) => return Err(anyhow!("[lilRouter: FailedToDecodeOutput] {:?}", e)),
        },
        UniswapV3(_) => match decode_swap_v3_result(output.into()) {
            Ok(output) => output,
            Err(e) => return Err(anyhow!("[lilRouter: FailedToDecodeOutput] {:?}", e)),
        },
    };

    let revenue = post_sandwich_balance
        .checked_sub((*WETH_FUND_AMT).into())
        .unwrap_or_default();

    Ok(revenue)
}

/// Inserts custom minimal router contract into evm instance for simulations
fn inject_lil_router_code(db: &mut CacheDB<SharedBackend>) {
    // insert lilRouter bytecode
    let lil_router_info = AccountInfo::new(
        rU256::ZERO,
        0,
        Bytecode::new_raw((*LIL_ROUTER_CODE.0).into()),
    );
    db.insert_account_info(*LIL_ROUTER_ADDRESS, lil_router_info);

    // insert and fund lilRouter controller (so we can spoof)
    let controller_info = AccountInfo::new(*WETH_FUND_AMT, 0, Bytecode::default());
    db.insert_account_info(*LIL_ROUTER_CONTROLLER, controller_info);

    // fund lilRouter with 200 weth
    let slot = keccak256(&abi::encode(&[
        abi::Token::Address((*LIL_ROUTER_ADDRESS).into()),
        abi::Token::Uint(U256::from(3)),
    ]));

    db.insert_account_storage((*WETH_ADDRESS).into(), slot.into(), eth_to_wei(200))
        .unwrap();
}
