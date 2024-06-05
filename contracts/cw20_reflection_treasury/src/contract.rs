use std::ops::{Div, Mul, Sub};

#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    coin, from_json, to_json_binary, Addr, Api, Binary, Decimal, Deps, DepsMut, Env, MessageInfo,
    QuerierWrapper, QueryRequest, Response, StdError, StdResult, Storage, Uint128, WasmMsg,
    WasmQuery,
};

use cw20::{BalanceResponse, Cw20ExecuteMsg};
use dojoswap::pair::SimulationResponse;

use cw2::set_contract_version;

use crate::msg::{
    Cw20HookMsg, Cw20ReceiveMsg, ExecuteMsg, InstantiateMsg, MigrateMsg, QueryMsg, TokenQueryMsg,
};
use cw20_base::ContractError;
use cw_storage_plus::Item;
use dojoswap::asset::{Asset, AssetInfo, PairInfo};
use dojoswap::pair::QueryMsg as PairQueryMsg;

// version info for migration info
const CONTRACT_NAME: &str = "dojoswap:reflection";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const MIN_LIQUIFY_AMT: Item<Uint128> = Item::new("min_liquify_amt"); // minimum number of babyTOKEN before liquifying

pub const ADMIN: Item<String> = Item::new("admin");
pub const TOKEN: Item<Addr> = Item::new("token");
pub const ROUTER: Item<String> = Item::new("router");
pub const LIQUIDTY_TOKEN: Item<String> = Item::new("liquidity_token");
pub const LIQUIDITY_PAIR_CONTRACT: Item<String> = Item::new("liquidity_pair_contract");
pub const REFLECTION_PAIR_CONTRACT: Item<String> = Item::new("reflection_pair_contract");
pub const LIQUIDITY_PAIR: Item<[AssetInfo; 2]> = Item::new("liquidity_pair");
pub const REFLECTION_PAIR: Item<[AssetInfo; 2]> = Item::new("reflection_pair");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    deps.api.addr_validate(&msg.admin.to_string())?;
    deps.api.addr_validate(&msg.router.to_string())?;
    ADMIN.save(deps.storage, &msg.admin.to_string())?;
    ROUTER.save(deps.storage, &msg.router.to_string())?;
    TOKEN.save(deps.storage, &msg.token)?;
    MIN_LIQUIFY_AMT.save(deps.storage, &Uint128::zero())?;

    Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::Receive(msg) => {
            receive_cw20(&deps.querier, deps.storage, deps.api, env, info, msg)
        }
        // Reflection features
        ExecuteMsg::SetReflectionPair {
            asset_infos,
            pair_contract,
        } => set_reflection_pair(deps, env, info, asset_infos, pair_contract),
        ExecuteMsg::SetLiquidityPair {
            asset_infos,
            pair_contract,
        } => set_liquidity_pair(deps, env, info, asset_infos, pair_contract),
        ExecuteMsg::SetMinLiquify { min_liquify_amt } => {
            set_min_liquify_amt(deps, env, info, min_liquify_amt)
        }
        // ExecuteMsg::SetToken { address } => set_token(deps, env, info, address),
        ExecuteMsg::Liquify {} => liquify_treasury(&deps.querier, env, deps.storage),
        ExecuteMsg::WithdrawToken { token } => withdraw_token(deps, env, info, token),
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Balance {} => {
            let token: Addr = TOKEN.load(deps.storage)?;
            to_json_binary(&query_balance(&deps.querier, token, env.contract.address)?)
        }
    }
}

pub fn receive_cw20(
    querier: &QuerierWrapper,
    storage: &mut dyn Storage,
    api: &dyn Api,
    env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> Result<Response, ContractError> {
    let token = TOKEN.may_load(storage)?.unwrap();

    match from_json(&cw20_msg.msg) {
        Ok(Cw20HookMsg::Liquify {}) => {
            // only token contract can execute this message
            if token.to_string() != api.addr_validate(info.sender.as_str())? {
                return Err(ContractError::Unauthorized {});
            }

            liquify_treasury(&querier, env.clone(), storage)
        }
        Err(_) => Err(ContractError::Unauthorized {}),
    }
}

/// Core function of the treasury. Will be used to liquify, burn, and reflect tokens in one operation
/// 1. Liquify babyTOKEN into LP tokens
/// 2. Reflect babyTOKEN into DOJO to be sent into fee collector wallet
/// 3. Burn a portion of babyTOKEN
pub fn liquify_treasury(
    querier: &QuerierWrapper,
    env: Env,
    storage: &mut dyn Storage,
) -> Result<Response, ContractError> {
    let querier = querier.clone();

    let router = ROUTER.may_load(storage)?.unwrap_or_default();
    // let admin = ADMIN.may_load(storage)?.unwrap_or_default();
    let token = TOKEN.load(storage)?;
    let contract_balance = query_balance(&querier, token.clone(), env.contract.address.clone())?;

    let liquidity_pair = LIQUIDITY_PAIR.may_load(storage)?.unwrap();
    let liquidity_pair_contract = LIQUIDITY_PAIR_CONTRACT.may_load(storage)?.unwrap();
    let reflection_pair = REFLECTION_PAIR.may_load(storage)?.unwrap();
    let min_liquify_amt = MIN_LIQUIFY_AMT
        .may_load(storage)?
        .unwrap_or(Uint128::zero());

    // Short circuit if there's not enough contract balance to liquify
    if contract_balance < min_liquify_amt {
        return Ok(Response::default());
    }

    // Loads all the tax rates from the modified CW20 token
    let (_tax_rate, reflection_rate, burn_rate, _transfer_rate): (
        Decimal,
        Decimal,
        Decimal,
        Decimal,
    ) = querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
        contract_addr: token.to_string(),
        msg: to_json_binary(&TokenQueryMsg::QueryRates {})?,
    }))?;

    let mut messages: Vec<WasmMsg> = vec![];

    let reflect_amt = contract_balance.mul(reflection_rate);
    let burn_amt = contract_balance.mul(burn_rate);
    let liquidity_amt = contract_balance.sub(reflect_amt).sub(burn_amt);
    // Taxes - 100000
    // Reflection - 50000
    // Burn - 10000
    // Liq amt - 40000
    if liquidity_amt > Uint128::zero() {
        // Swaps half of babyTOKEN into INJ
        let swap_amount = liquidity_amt.div(Uint128::from(2u128));
        // Increases allowance of babyTOKEN to liquidity pair contract (allows adding liquidity)
        messages.push(WasmMsg::Execute {
            contract_addr: token.to_string(),
            msg: to_json_binary(&cw20::Cw20ExecuteMsg::IncreaseAllowance {
                spender: liquidity_pair_contract.clone(),
                amount: liquidity_amt.sub(swap_amount),
                expires: None,
            })?,
            funds: vec![],
        });

        // Simulates swapping of half of babyTOKEN into INJ
        let simulation = simulate(
            &querier,
            liquidity_pair_contract.clone(),
            &Asset {
                amount: swap_amount,
                info: liquidity_pair[0].clone(),
            },
        )?;
        // We formulate a swap message to swap babyTOKEN into INJ
        messages.push(WasmMsg::Execute {
            contract_addr: token.to_string(),
            msg: to_json_binary(&cw20::Cw20ExecuteMsg::Send {
                contract: liquidity_pair_contract.to_string(),
                amount: swap_amount,
                msg: to_json_binary(&dojoswap::pair::Cw20HookMsg::Swap {
                    belief_price: None,
                    max_spread: None,
                    to: None,
                    deadline: None,
                })?,
            })?,
            funds: vec![],
        });

        // Formulate variable to allow us to add liquidity to the pool
        let assets: [Asset; 2] = [
            Asset {
                amount: liquidity_amt.sub(swap_amount), // add remaining amount of babyTOKEN as liquidity
                info: liquidity_pair[0].clone(),        // babyTOKEN
            },
            Asset {
                amount: simulation.return_amount, // add simulated INJ return amount to be added as liquidity
                info: liquidity_pair[1].clone(),  // INJ
            },
        ];

        // We formulate a ProvideLiquidity message to add babyTOKEN liquidity to the pool
        match reflection_pair[1].clone() {
            AssetInfo::NativeToken { denom } => {
                // If the asset is a native token, we provide liquidity via a denom message
                messages.push(WasmMsg::Execute {
                    contract_addr: liquidity_pair_contract.to_string(),
                    msg: to_json_binary(&dojoswap::pair::ExecuteMsg::ProvideLiquidity {
                        assets,
                        receiver: None,
                        deadline: None,
                        slippage_tolerance: None,
                    })?,
                    funds: vec![coin(simulation.return_amount.u128(), denom)],
                });
            }
            AssetInfo::Token { contract_addr } => {
                // If asset is a CW20, we provide liquidity via increase allowance message
                messages.push(WasmMsg::Execute {
                    contract_addr,
                    msg: to_json_binary(&Cw20ExecuteMsg::IncreaseAllowance {
                        spender: liquidity_pair_contract.to_string(),
                        amount: simulation.return_amount,
                        expires: None,
                    })?,
                    funds: vec![],
                });
                messages.push(WasmMsg::Execute {
                    contract_addr: liquidity_pair_contract.to_string(),
                    msg: to_json_binary(&dojoswap::pair::ExecuteMsg::ProvideLiquidity {
                        assets,
                        receiver: None,
                        deadline: None,
                        slippage_tolerance: None,
                    })?,
                    funds: vec![],
                });
            }
        };
    }

    if reflect_amt > Uint128::zero() {
        // 1. swap babyToken into INJ
        // 2. swap INJ into reflection target token (DOJO)
        // 3. sends reflection token to fee collector
        let operations = vec![
            dojoswap::router::SwapOperation::DojoSwap {
                offer_asset_info: dojoswap::asset::AssetInfo::Token {
                    contract_addr: token.to_string(),
                },
                ask_asset_info: reflection_pair[1].clone(),
            },
            dojoswap::router::SwapOperation::DojoSwap {
                offer_asset_info: reflection_pair[1].clone(),
                ask_asset_info: reflection_pair[0].clone(),
            },
        ];
        // Executes a sell of babyTOKEN into INJ, then INJ into reflection target token (DOJO) via router contract
        messages.push(WasmMsg::Execute {
            contract_addr: token.to_string(),
            msg: to_json_binary(&cw20::Cw20ExecuteMsg::Send {
                contract: router.to_string(),
                amount: reflect_amt,
                msg: to_json_binary(&dojoswap::router::ExecuteMsg::ExecuteSwapOperations {
                    operations,
                    minimum_receive: None,
                    to: None, // reflected token is sent here into treasury
                    deadline: None,
                })?,
            })?,
            funds: vec![],
        });
    }

    if burn_amt > Uint128::zero() {
        // Burns babyTOKEN
        messages.push(WasmMsg::Execute {
            contract_addr: token.to_string(),
            msg: to_json_binary(&cw20::Cw20ExecuteMsg::Burn { amount: burn_amt })?,
            funds: vec![],
        });
    }

    let res = Response::new().add_messages(messages);

    Ok(res)
}

/// Used to simulate swap operations against DojoSwap pair
pub fn simulate(
    querier: &QuerierWrapper,
    pair_contract: String,
    offer_asset: &Asset,
) -> StdResult<SimulationResponse> {
    querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
        contract_addr: pair_contract,
        msg: to_json_binary(&PairQueryMsg::Simulation {
            offer_asset: offer_asset.clone(),
        })?,
    }))
}

pub fn query_balance(querier: &QuerierWrapper, token: Addr, address: Addr) -> StdResult<Uint128> {
    let response: BalanceResponse = querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
        contract_addr: token.to_string(),
        msg: to_json_binary(&cw20::Cw20QueryMsg::Balance {
            address: address.to_string(),
        })?,
    }))?;
    Ok(response.balance)
}

// Check below for pair ordering
// 1. This contract address (babyToken)
// 2. The quote token (inj)
pub fn set_liquidity_pair(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    asset_infos: [AssetInfo; 2],
    pair_contract: String,
) -> Result<Response, ContractError> {
    ensure_admin(&deps, &info)?;
    let reflection_pair = REFLECTION_PAIR.load(deps.storage);
    LIQUIDITY_PAIR.save(deps.storage, &asset_infos)?;
    LIQUIDITY_PAIR_CONTRACT.save(deps.storage, &pair_contract)?;

    match reflection_pair {
        Err(_) => {}
        Ok(asset_info) => {
            let unbound = asset_info;
            let reflect_1 = unbound.get(1).unwrap();
            if !reflect_1.eq(&asset_infos[1]) {
                return Err(ContractError::Std(StdError::generic_err(
                    "asset_infos[1] do not match",
                )));
            }
        }
    };

    let response: PairInfo = deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
        contract_addr: pair_contract,
        msg: to_json_binary(&PairQueryMsg::Pair {})?,
    }))?;

    match response.asset_infos[0].clone() {
        AssetInfo::Token { contract_addr } => {
            deps.api.addr_validate(&contract_addr.to_string())?;
        }
        AssetInfo::NativeToken { denom: _ } => {
            return Err(ContractError::Std(StdError::generic_err(
                "token should be cw20",
            )));
        }
    };

    LIQUIDTY_TOKEN.save(deps.storage, &response.liquidity_token.to_string())?;

    response
        .asset_infos
        .iter()
        .find(|info| info.equal(&asset_infos[0]))
        .ok_or(StdError::generic_err("asset_infos[0] is not valid"))?;

    response
        .asset_infos
        .iter()
        .find(|info| info.equal(&asset_infos[1]))
        .ok_or(StdError::generic_err("asset_infos[1] is not valid"))?;

    Ok(Response::default())
}

// Check below for pair ordering
// 1. The reflection token (DOJO)
// 2. The quote token (inj)
pub fn set_reflection_pair(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    asset_infos: [AssetInfo; 2],
    pair_contract: String,
) -> Result<Response, ContractError> {
    ensure_admin(&deps, &info)?;
    let liquidity_pair = LIQUIDITY_PAIR.load(deps.storage);
    REFLECTION_PAIR.save(deps.storage, &asset_infos)?;
    REFLECTION_PAIR_CONTRACT.save(deps.storage, &pair_contract)?;

    match liquidity_pair {
        Err(_) => {}
        Ok(asset_info) => {
            let unbound = asset_info;
            let liquidity_1 = unbound.get(1).unwrap();
            if !liquidity_1.eq(&asset_infos[1]) {
                return Err(ContractError::Std(StdError::generic_err(
                    "asset_infos[1] do not match",
                )));
            }
        }
    };

    let response: PairInfo = deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
        contract_addr: pair_contract,
        msg: to_json_binary(&PairQueryMsg::Pair {})?,
    }))?;

    response
        .asset_infos
        .iter()
        .find(|info| info.equal(&asset_infos[0]))
        .ok_or(StdError::generic_err("asset_infos[0] is not valid"))?;

    response
        .asset_infos
        .iter()
        .find(|info| info.equal(&asset_infos[1]))
        .ok_or(StdError::generic_err("asset_infos[1] is not valid"))?;

    Ok(Response::default())
}

/// Sets minimum babyTOKEN required to liquify
pub fn set_min_liquify_amt(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    min_liquify_amt: Uint128,
) -> Result<Response, ContractError> {
    ensure_admin(&deps, &info)?;

    MIN_LIQUIFY_AMT.save(deps.storage, &min_liquify_amt)?;
    Ok(Response::default())
}

/// Withdraws a token of your choice from contract, but not allowed to withdraw LP
pub fn withdraw_token(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    token: Addr,
) -> Result<Response, ContractError> {
    ensure_admin(&deps, &info)?;
    // Prevents liquidity token from being removed
    if token.to_string() == LIQUIDTY_TOKEN.may_load(deps.storage)?.unwrap_or_default() {
        return Err(ContractError::Std(StdError::generic_err(
            "Unauthorized: not allowed to withdraw LP",
        )));
    }

    let response: cw20::BalanceResponse = deps.querier.query_wasm_smart(
        token.clone(),
        &cw20::Cw20QueryMsg::Balance {
            address: env.contract.address.to_string(),
        },
    )?;

    let res = Response::new()
        .add_attribute("withdraw_token", response.balance)
        .add_message(WasmMsg::Execute {
            contract_addr: token.to_string(),
            msg: to_json_binary(&cw20::Cw20ExecuteMsg::Transfer {
                recipient: info.sender.to_string(),
                amount: response.balance,
            })?,
            funds: vec![],
        });

    Ok(res)
}

/// Ensures only admins can use this function
pub fn ensure_admin(deps: &DepsMut, info: &MessageInfo) -> Result<Response, ContractError> {
    let admin = ADMIN.may_load(deps.storage)?.unwrap_or_default();
    if info.sender != admin {
        return Err(ContractError::Std(StdError::generic_err(
            "Unauthorized: not admin",
        )));
    }

    Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(_deps: DepsMut, _env: Env, _msg: MigrateMsg) -> Result<Response, ContractError> {
    Ok(Response::default())
}
