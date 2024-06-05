use std::ops::{Mul, Sub};
use std::str::FromStr;

#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    attr, to_json_binary, Binary, CosmosMsg, Decimal, Deps, DepsMut, Env, MessageInfo, 
    Response, StdError, StdResult, Storage,   Uint128, WasmMsg,
};

use cw2::set_contract_version;
use cw20::{Cw20ReceiveMsg, Logo, LogoInfo, MarketingInfoResponse};
use cw20_base::allowances::{
    deduct_allowance, execute_burn_from, execute_decrease_allowance, execute_increase_allowance,
    query_allowance,
};
use cw20_base::contract::{
    create_accounts, execute_burn, execute_mint, execute_update_marketing, execute_upload_logo,
    query_balance, query_download_logo, query_marketing_info, query_minter, query_token_info,
};
use cw20_base::enumerable::{query_all_accounts, query_all_allowances};

use crate::msg::{
    ExecuteMsg, InstantiateMsg, MigrateMsg, QueryMsg, QueryTaxResponse, TreasuryExecuteMsg,
};
use cw20_base::state::{MinterData, TokenInfo, BALANCES, LOGO, MARKETING_INFO, TOKEN_INFO};
use cw20_base::ContractError;
use cw_storage_plus::{Item, Map};

// version info for migration info
const CONTRACT_NAME: &str = "qtum:reflection";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const TAX_RATE: Item<Decimal> = Item::new("tax_rate");
pub const REFLECTION_RATE: Item<Decimal> = Item::new("reflection_rate");
pub const BURN_RATE: Item<Decimal> = Item::new("burn_rate");
pub const MAX_TRANSFER_SUPPLY_RATE: Item<Decimal> = Item::new("max_transfer_supply_rate");

pub const ADMIN: Item<String> = Item::new("admin");
pub const LAST_LIQUIFY: Item<u64> = Item::new("last_liquify");
pub const TREASURY: Item<String> = Item::new("treasury");
pub const PAIRLIST: Map<String, bool> = Map::new("pairlist");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    mut deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    // check valid token info
    msg.validate()?;

    ADMIN.save(deps.storage, &info.sender.to_string())?;

    TAX_RATE.save(deps.storage, &Decimal::zero())?;
    REFLECTION_RATE.save(deps.storage, &Decimal::zero())?;
    BURN_RATE.save(deps.storage, &Decimal::zero())?;
    MAX_TRANSFER_SUPPLY_RATE.save(deps.storage, &Decimal::from_str("1")?)?;
    PAIRLIST.save(deps.storage, info.sender.to_string(), &true)?;

    // create initial accounts
    let total_supply = create_accounts(&mut deps, &msg.initial_balances)?;

    if let Some(limit) = msg.get_cap() {
        if total_supply > limit {
            return Err(ContractError::Std(StdError::generic_err(
                "Initial supply greater than cap",
            )));
        }
    }

    let mint = match msg.mint {
        Some(m) => Some(MinterData {
            minter: deps.api.addr_validate(&m.minter)?,
            cap: m.cap,
        }),
        None => None,
    };

    // store token info
    let data = TokenInfo {
        name: msg.name,
        symbol: msg.symbol,
        decimals: msg.decimals,
        total_supply,
        mint,
    };

    if let Some(marketing) = msg.marketing {
        let logo = if let Some(logo) = marketing.logo {
            LOGO.save(deps.storage, &logo)?;

            match logo {
                Logo::Url(url) => Some(LogoInfo::Url(url)),
                Logo::Embedded(_) => Some(LogoInfo::Embedded),
            }
        } else {
            None
        };

        let marketing_data = MarketingInfoResponse {
            project: marketing.project,
            description: marketing.description,
            marketing: Some(deps.api.addr_validate(&marketing.marketing.unwrap())?),
            logo,
        };
        MARKETING_INFO.save(deps.storage, &marketing_data)?;
    }

    TOKEN_INFO.save(deps.storage, &data)?;

    Ok(Response::default())
}

/// Standard CW20 transfer function that is modified to include tax functions
/// These modifications are all applied to the `transfer`, `send`, `transfer_from`, and `send_from` functions
pub fn execute_transfer(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    recipient: String,
    amount: Uint128,
) -> Result<Response, ContractError> {
    if amount == Uint128::zero() {
        return Err(ContractError::InvalidZeroAmount {});
    }
    // If whitelisetd, we simply do not apply taxes
    let to_pair = PAIRLIST
        .may_load(deps.storage, recipient.clone())?
        .unwrap_or_default();
    let from_pair = PAIRLIST
        .may_load(deps.storage, info.sender.to_string())?
        .unwrap_or_default();
    let is_pair = to_pair || from_pair;

    // Loads treasury addresses, and query for taxes on transfers
    let treasury = TREASURY.may_load(deps.storage)?.unwrap_or_default();
    let rcpt_addr = deps.api.addr_validate(&recipient)?;
    let taxes = query_tax(deps.storage, amount)?;
    let outgoing_amount = if is_pair { taxes.after_tax } else { amount };

    BALANCES.update(
        deps.storage,
        &info.sender,
        |balance: Option<Uint128>| -> StdResult<_> {
            Ok(balance.unwrap_or_default().checked_sub(amount)?)
        },
    )?;
    BALANCES.update(
        deps.storage,
        &rcpt_addr,
        |balance: Option<Uint128>| -> StdResult<_> {
            Ok(balance.unwrap_or_default() + outgoing_amount)
        },
    )?;

    let mut messages = vec![];

    // we apply taxes, and immediately add them to the treasury by modifying balance variables
    // We also send generate a transfer teransaction log under `TransferEvent` to ensure explorer tracks transfer properly
    if is_pair {
        BALANCES.update(
            deps.storage,
            &deps.api.addr_validate(&treasury)?,
            |balance: Option<Uint128>| -> StdResult<_> {
                Ok(balance.unwrap_or_default() + taxes.taxed_amount)
            },
        )?;

        messages.push(WasmMsg::Execute {
            contract_addr: env.contract.address.to_string(),
            msg: to_json_binary(&ExecuteMsg::TransferEvent {
                from: info.sender.to_string(),
                to: treasury.to_string(),
                amount: taxes.taxed_amount,
            })?,
            funds: vec![],
        })
    }

    let res = Response::new()
        .add_messages(messages)
        .add_attribute("action", "transfer")
        .add_attribute("from", info.sender)
        .add_attribute("to", recipient)
        .add_attribute("amount", outgoing_amount);
    Ok(res)
}

pub fn execute_send(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    contract: String,
    amount: Uint128,
    msg: Binary,
) -> Result<Response, ContractError> {
    if amount == Uint128::zero() {
        return Err(ContractError::InvalidZeroAmount {});
    }
    let to_pair = PAIRLIST
        .may_load(deps.storage, contract.clone())?
        .unwrap_or_default();
    let from_pair = PAIRLIST
        .may_load(deps.storage, info.sender.to_string())?
        .unwrap_or_default();
    let is_pair = to_pair || from_pair;
    let treasury = TREASURY.may_load(deps.storage)?.unwrap_or_default();
    let rcpt_addr = deps.api.addr_validate(&contract)?;
    let taxes = query_tax(deps.storage, amount)?;
    let outgoing_amount = if is_pair { taxes.after_tax } else { amount };

    // move the tokens to the contract
    BALANCES.update(
        deps.storage,
        &info.sender,
        |balance: Option<Uint128>| -> StdResult<_> {
            Ok(balance.unwrap_or_default().checked_sub(amount)?)
        },
    )?;
    BALANCES.update(
        deps.storage,
        &rcpt_addr,
        |balance: Option<Uint128>| -> StdResult<_> {
            Ok(balance.unwrap_or_default() + outgoing_amount)
        },
    )?;

    let mut messages = vec![];

    if is_pair {
        BALANCES.update(
            deps.storage,
            &deps.api.addr_validate(&treasury)?,
            |balance: Option<Uint128>| -> StdResult<_> {
                Ok(balance.unwrap_or_default() + taxes.taxed_amount)
            },
        )?;

        messages.push(WasmMsg::Execute {
            contract_addr: env.contract.address.to_string(),
            msg: to_json_binary(&ExecuteMsg::TransferEvent {
                from: info.sender.to_string(),
                to: treasury.to_string(),
                amount: taxes.taxed_amount,
            })?,
            funds: vec![],
        })
    }

    let res = Response::new()
        .add_messages(messages)
        .add_attribute("action", "send")
        .add_attribute("from", &info.sender)
        .add_attribute("to", &contract)
        .add_attribute("amount", outgoing_amount)
        .add_message(
            // We do not modify the send message, but we allow the hooked contract to calculate taxes against this contract
            Cw20ReceiveMsg {
                sender: info.sender.into(),
                amount: outgoing_amount,
                msg,
            }
            .into_cosmos_msg(contract)?,
        );
    Ok(res)
}

pub fn execute_transfer_from(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    owner: String,
    recipient: String,
    amount: Uint128,
) -> Result<Response, ContractError> {
    
    let to_pair = PAIRLIST
        .may_load(deps.storage, recipient.clone())?
        .unwrap_or_default();
    let from_pair = PAIRLIST
        .may_load(deps.storage, info.sender.to_string())?
        .unwrap_or_default();
    let is_pair = to_pair || from_pair;
    let treasury = TREASURY.may_load(deps.storage)?.unwrap_or_default();
    let rcpt_addr = deps.api.addr_validate(&recipient)?;
    let owner_addr = deps.api.addr_validate(&owner)?;
    let taxes = query_tax(deps.storage, amount)?;
    let outgoing_amount = if is_pair { taxes.after_tax } else { amount };

    // deduct allowance before doing anything else have enough allowance
    deduct_allowance(deps.storage, &owner_addr, &info.sender, &env.block, amount)?;

    BALANCES.update(
        deps.storage,
        &owner_addr,
        |balance: Option<Uint128>| -> StdResult<_> {
            Ok(balance.unwrap_or_default().checked_sub(amount)?)
        },
    )?;
    BALANCES.update(
        deps.storage,
        &rcpt_addr,
        |balance: Option<Uint128>| -> StdResult<_> {
            Ok(balance.unwrap_or_default() + outgoing_amount)
        },
    )?;

    let mut messages = vec![];
    if is_pair {
        BALANCES.update(
            deps.storage,
            &deps.api.addr_validate(&treasury)?,
            |balance: Option<Uint128>| -> StdResult<_> {
                Ok(balance.unwrap_or_default() + taxes.taxed_amount)
            },
        )?;

        messages.push(WasmMsg::Execute {
            contract_addr: env.contract.address.to_string(),
            msg: to_json_binary(&ExecuteMsg::TransferEvent {
                from: info.sender.to_string(),
                to: treasury.to_string(),
                amount: taxes.taxed_amount,
            })?,
            funds: vec![],
        })
    }

    let res = Response::new().add_messages(messages).add_attributes(vec![
        attr("action", "transfer_from"),
        attr("from", owner),
        attr("to", recipient),
        attr("by", info.sender),
        attr("amount", outgoing_amount),
    ]);
    Ok(res)
}

pub fn execute_send_from(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    owner: String,
    contract: String,
    amount: Uint128,
    msg: Binary,
) -> Result<Response, ContractError> {
   
    let to_pair = PAIRLIST
        .may_load(deps.storage, contract.clone())?
        .unwrap_or_default();
    let from_pair = PAIRLIST
        .may_load(deps.storage, info.sender.to_string())?
        .unwrap_or_default();
    let is_pair = to_pair || from_pair ;
    let treasury = TREASURY.may_load(deps.storage)?.unwrap_or_default();
    let rcpt_addr = deps.api.addr_validate(&contract)?;
    let owner_addr = deps.api.addr_validate(&owner)?;
    let taxes = query_tax(deps.storage, amount)?;
    let outgoing_amount = if is_pair { taxes.after_tax  } else { amount };

    // deduct allowance before doing anything else have enough allowance
    deduct_allowance(deps.storage, &owner_addr, &info.sender, &env.block, amount)?;

    // move the tokens to the contract
    BALANCES.update(
        deps.storage,
        &owner_addr,
        |balance: Option<Uint128>| -> StdResult<_> {
            Ok(balance.unwrap_or_default().checked_sub(amount)?)
        },
    )?;
    BALANCES.update(
        deps.storage,
        &rcpt_addr,
        |balance: Option<Uint128>| -> StdResult<_> {
            Ok(balance.unwrap_or_default() + outgoing_amount)
        },
    )?;

    let mut messages = vec![];
    if is_pair  {
        BALANCES.update(
            deps.storage,
            &deps.api.addr_validate(&treasury)?,
            |balance: Option<Uint128>| -> StdResult<_> {
                Ok(balance.unwrap_or_default() + taxes.taxed_amount)
            },
        )?;

        messages.push(WasmMsg::Execute {
            contract_addr: env.contract.address.to_string(),
            msg: to_json_binary(&ExecuteMsg::TransferEvent {
                from: info.sender.to_string(),
                to: treasury.to_string(),
                amount: taxes.taxed_amount,
            })?,
            funds: vec![],
        })
    }

    let attrs = vec![
        attr("action", "send_from"),
        attr("from", &owner),
        attr("to", &contract),
        attr("by", &info.sender),
        attr("amount", outgoing_amount),
    ];

    // create a send message
    let msg = Cw20ReceiveMsg {
        sender: info.sender.clone().into(),
        amount: outgoing_amount,
        msg,
    }
    .into_cosmos_msg(contract)?;

    let res = Response::new()
        .add_messages(messages)
        .add_message(msg)
        .add_attributes(attrs);
    Ok(res)
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::Transfer { recipient, amount } => {
            execute_transfer(deps, env, info, recipient, amount)
        }
        ExecuteMsg::Burn { amount } => execute_burn(deps, env, info, amount),
        ExecuteMsg::Send {
            contract,
            amount,
            msg,
        } => execute_send(deps, env, info, contract, amount, msg),
        ExecuteMsg::Mint { recipient, amount } => execute_mint(deps, env, info, recipient, amount),
        ExecuteMsg::IncreaseAllowance {
            spender,
            amount,
            expires,
        } => execute_increase_allowance(deps, env, info, spender, amount, expires),
        ExecuteMsg::DecreaseAllowance {
            spender,
            amount,
            expires,
        } => execute_decrease_allowance(deps, env, info, spender, amount, expires),
        ExecuteMsg::TransferFrom {
            owner,
            recipient,
            amount,
        } => execute_transfer_from(deps, env, info, owner, recipient, amount),
        ExecuteMsg::BurnFrom { owner, amount } => execute_burn_from(deps, env, info, owner, amount),
        ExecuteMsg::SendFrom {
            owner,
            contract,
            amount,
            msg,
        } => execute_send_from(deps, env, info, owner, contract, amount, msg),
        ExecuteMsg::UpdateMarketing {
            project,
            description,
            marketing,
        } => execute_update_marketing(deps, env, info, project, description, marketing),
        ExecuteMsg::UploadLogo(logo) => execute_upload_logo(deps, env, info, logo),

        // Reflection features
        ExecuteMsg::SetPair { contract, enable } => set_pairlist(deps, info, contract, enable),
        ExecuteMsg::SetTaxRate {
            global_rate,
            reflection_rate,
            burn_rate,
        } => set_tax_rate(
            deps,
            env,
            info,
            global_rate,
            reflection_rate,
            burn_rate
        ),
        ExecuteMsg::TransferEvent { from, to, amount } => {
            generate_transfer_event(deps, info, env, from, to, amount)
        }
        ExecuteMsg::MigrateTreasury { code_id } => migrate_treasury(deps, env, info, code_id),
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Balance { address } => to_json_binary(&query_balance(deps, address)?),
        QueryMsg::TokenInfo {} => to_json_binary(&query_token_info(deps)?),
        QueryMsg::Minter {} => to_json_binary(&query_minter(deps)?),
        QueryMsg::Allowance { owner, spender } => {
            to_json_binary(&query_allowance(deps, owner, spender)?)
        }
        QueryMsg::AllAllowances {
            owner,
            start_after,
            limit,
        } => to_json_binary(&query_all_allowances(deps, owner, start_after, limit)?),
        QueryMsg::AllAccounts { start_after, limit } => {
            to_json_binary(&query_all_accounts(deps, start_after, limit)?)
        }
        QueryMsg::MarketingInfo {} => to_json_binary(&query_marketing_info(deps)?),
        QueryMsg::DownloadLogo {} => to_json_binary(&query_download_logo(deps)?),
        QueryMsg::QueryTax { amount } => to_json_binary(&query_tax(deps.storage, amount)?),
        QueryMsg::QueryRates {} => to_json_binary(&query_rate(deps.storage)?),
        QueryMsg::GetWhitelist { address } => {
            to_json_binary(&query_pairlist(deps.storage, address)?)
        }
    }
}

/// Used to calculate the amount of taxes to be paid, to be used in all transfer functions
pub fn query_tax(storage: &dyn Storage, amount: Uint128) -> Result<QueryTaxResponse, StdError> {
    let reflection_rate = REFLECTION_RATE.may_load(storage)?.unwrap();
    let tax_rate = TAX_RATE.may_load(storage)?.unwrap();
    let burn_rate = BURN_RATE.may_load(storage)?.unwrap();

    let taxed_amount = amount.mul(tax_rate);
    let after_tax = amount.sub(taxed_amount);
    let reflection_amount = taxed_amount.mul(reflection_rate);
    let burn_amount = taxed_amount.mul(burn_rate);
    let liquidity_amount = taxed_amount.sub(reflection_amount).sub(burn_amount);

    Ok(QueryTaxResponse {
        taxed_amount,
        after_tax,
        reflection_amount,
        liquidity_amount,
    })
}

/// Returns the current tax rates
pub fn query_rate(storage: &dyn Storage) -> Result<(Decimal, Decimal, Decimal), StdError> {
    let tax_rate = TAX_RATE.may_load(storage)?.unwrap();
    let reflection_rate = REFLECTION_RATE.may_load(storage)?.unwrap();
    let burn_rate = BURN_RATE.may_load(storage)?.unwrap();

    Ok((tax_rate, reflection_rate, burn_rate))
}

pub fn query_pairlist(storage: &dyn Storage, address: String) -> Result<bool, StdError> {
    let pairlist = PAIRLIST.may_load(storage, address)?.unwrap();

    Ok(pairlist)
}

/// Global rate is number between 0 to 1. 0.1 refers to 10% taxes on all transfers
/// Reflection rate is number between 0 to 1. 0.5 refers to 50% of GLOBAL taxes gets transferred as reflection
/// Burn rate is number between 0 to 1. 0.1 refers to 10% of GLOBAL taxes gets burnt
/// Antiwhale rate is number between 0 to 1. 0.02 refers to when someone intends to move 2% of supply, anti-whale gets triggered
pub fn set_tax_rate(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    global_rate: Decimal,
    reflection_rate: Decimal,
    burn_rate: Decimal,
) -> Result<Response, ContractError> {
    ensure_admin(&deps, &info)?;

    if global_rate > Decimal::one() {
        return Err(ContractError::Std(StdError::generic_err(
            "global_rate must be <= 1",
        )));
    }

    if reflection_rate + burn_rate > Decimal::one() {
        return Err(ContractError::Std(StdError::generic_err(
            "addition of reflection_rate & burn_rate must be <= 1",
        )));
    }

    TAX_RATE.save(deps.storage, &global_rate)?;
    REFLECTION_RATE.save(deps.storage, &reflection_rate)?;
    BURN_RATE.save(deps.storage, &burn_rate)?;
    Ok(Response::default())
}

/// Sets pair address (taxed)
pub fn set_pairlist(
    deps: DepsMut,
    info: MessageInfo,
    user: String,
    enable: bool,
) -> Result<Response, ContractError> {
    ensure_admin(&deps, &info)?;
    deps.api.addr_validate(&user.to_string())?;
    PAIRLIST.save(deps.storage, user.to_string(), &enable)?;
    Ok(Response::default())
}

/// This is used to ensure that only the admin can execute certain functions
pub fn ensure_admin(deps: &DepsMut, info: &MessageInfo) -> Result<Response, ContractError> {
    let admin = ADMIN.may_load(deps.storage)?.unwrap_or_default();
    if info.sender != admin {
        return Err(ContractError::Std(StdError::generic_err(
            "Unauthorized: not admin",
        )));
    }

    Ok(Response::default())
}


/// This is used to generate a transfer event to treasury contract (so that explorer tracks transfer events properly, and balances shows up correctly)
/// This is also used to trigger liquify (every 10 seconds) -> prevents recursive liquify that can cause out of gas
pub fn generate_transfer_event(
    deps: DepsMut,
    info: MessageInfo,
    env: Env,
    from: String,
    to: String,
    amount: Uint128,
) -> Result<Response, ContractError> {
    let last_liquify = LAST_LIQUIFY.may_load(deps.storage)?.unwrap_or_default();
    if info.sender.to_string() != env.contract.address.to_string() {
        return Err(ContractError::Std(StdError::generic_err(
            "Unauthorized: not contract",
        )));
    }
    let mut messages = vec![];
    // Allowed to liquify every 1 seconds
    if env.block.time.seconds() > last_liquify + 1 {
        LAST_LIQUIFY.save(deps.storage, &env.block.time.seconds())?;
        let treasury = TREASURY.may_load(deps.storage)?.unwrap_or_default();
        let liquify_msg = WasmMsg::Execute {
            contract_addr: treasury.to_string(),
            msg: to_json_binary(&TreasuryExecuteMsg::Liquify {})?,
            funds: vec![],
        };
        messages.push(liquify_msg);
    }

    let res = Response::new()
        .add_messages(messages)
        .add_attribute("action", "transfer")
        .add_attribute("from", from)
        .add_attribute("to", to)
        .add_attribute("amount", amount);
    Ok(res)
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(_deps: DepsMut, _env: Env, _msg: MigrateMsg) -> Result<Response, ContractError> {
    Ok(Response::default())
}

pub fn migrate_treasury(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    code_id: u64,
) -> Result<Response, ContractError> {
    let treasury = TREASURY.load(deps.storage)?;
    let admin = ADMIN.load(deps.storage)?;
    if info.sender.to_string() != admin.to_string() {
        return Err(ContractError::Std(StdError::generic_err("Not admin")));
    }

    Ok(
        Response::new().add_message(CosmosMsg::Wasm(WasmMsg::Migrate {
            contract_addr: treasury,
            new_code_id: code_id,
            msg: to_json_binary(&MigrateMsg {
                msg: "".to_string(),
            })?,
        })),
    )
}
