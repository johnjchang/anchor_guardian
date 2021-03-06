#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    to_binary, Binary, Deps, DepsMut, Env,
    MessageInfo, Response, StdError, StdResult, Uint128, Addr,
    WasmMsg, CosmosMsg, WasmQuery, QueryRequest, Coin, BankMsg
};
use cosmwasm_bignumber::{Decimal256, Uint256};
use anchor_guardian::cw20::{ExecuteMsg, InstantiateMsg, QueryMsg, ConfigResponse};
use cw20::{Cw20QueryMsg, AllowanceResponse, Cw20ExecuteMsg};
use crate::state::{CONFIG, STATE, BORROWERS, Config, State, Borrower, Guardian};
use terra_cosmwasm::TerraMsgWrapper;
use moneymarket::{
    overseer::{QueryMsg as OverseerQueryMsg, BorrowLimitResponse, CollateralsResponse, ConfigResponse as OverseerConfigResponse, ExecuteMsg as OverseerExecuteMsg},
    market::{QueryMsg as MarketQueryMsg, BorrowerInfoResponse, ExecuteMsg as MarketExecuteMsg},
    liquidation::{QueryMsg as LiquidationQueryMsg, LiquidationAmountResponse},
    oracle::PriceResponse,
    querier::{query_price, TimeConstraints},
};
use astroport::{
    pair::{QueryMsg as PairQueryMsg, ExecuteMsg as PairExecuteMsg, ReverseSimulationResponse, SimulationResponse, Cw20HookMsg as PairHookMsg},
    asset::{Asset, AssetInfo},
};
use std::cmp::min;
use smartwallet::wallet::ExecuteMsg as SmartWalletExecuteMsg;

const GUARDIAN_BUFFER: u64 = 10000000u64;


#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response<TerraMsgWrapper>> {

    let config = Config {
        owner: deps.api.addr_validate(&msg.owner)?,
        anchor_market_contract: deps.api.addr_validate(&msg.anchor_market_contract)?,
        anchor_overseer_contract: deps.api.addr_validate(&msg.anchor_overseer_contract)?,
        anchor_liquidation_contract: deps.api.addr_validate(&msg.anchor_liquidation_contract)?,
        anchor_oracle_contract: deps.api.addr_validate(&msg.anchor_oracle_contract)?,
        liquidator_fee: msg.liquidator_fee,
    };

    let state = State{
        whitelisted_cw20s: vec![],
    };

    STATE.save(deps.storage, &state)?;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> StdResult<Response<TerraMsgWrapper>> {
    match msg {
        ExecuteMsg::WhitelistCw20{address} => execute_whitelist_cw20(deps, info, address),
        ExecuteMsg::UpdateConfig {owner} => execute_update_config(deps, info, owner),
    
        //user funcs
        ExecuteMsg::AddGuardian { cw20_address, pair_address} => execute_add_guardian(deps, info, cw20_address, pair_address),
    
        //liquidator funcs
        ExecuteMsg::LiquidateCollateral { address } => execute_liquidate_collateral(deps, env, info, address),
    }
}


#[allow(clippy::too_many_arguments)]
pub fn execute_liquidate_collateral(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    address: String,
) -> StdResult<Response<TerraMsgWrapper>> {

    let config: Config = CONFIG.load(deps.storage)?;
    let mut attrs = vec![];

    //confirm valid address
    let borrower_addr: Addr = deps.api.addr_validate(&address)?;

    //fetch loan state
    let borrower_loan: BorrowerInfoResponse = deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart{
        contract_addr: config.anchor_market_contract.clone().into(),
        msg: to_binary(&MarketQueryMsg::BorrowerInfo{
            borrower: borrower_addr.clone().into(),
            block_height: Some(env.block.height)
        })?,
    }))?;

    attrs.push(("borrower_loan", borrower_loan.loan_amount.to_string()));

    let borrower_collateral: CollateralsResponse = deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart{
        contract_addr: config.anchor_overseer_contract.clone().into(),
        msg: to_binary(&OverseerQueryMsg::Collaterals{
            borrower: borrower_addr.clone().into(),
        })?,
    }))?;
    let borrower_limit: BorrowLimitResponse = deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart{
        contract_addr: config.anchor_overseer_contract.clone().into(),
        msg: to_binary(&OverseerQueryMsg::BorrowLimit{
            borrower: borrower_addr.clone().into(),
            block_time: Some(env.block.time.seconds()),
        })?,
    }))?;

    attrs.push(("borrower_limit", borrower_limit.borrow_limit.to_string()));

    if borrower_loan.loan_amount < borrower_limit.borrow_limit {
        return Err(StdError::generic_err("collateral ratio is safe"));
    }

    //fetch collateral prices and calc liquidation amount
    let mut prices = vec![];

    let overseer_config: OverseerConfigResponse = deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart{
        contract_addr: config.anchor_overseer_contract.clone().into(),
        msg: to_binary(&OverseerQueryMsg::Config{}
        )?,
    }))?;

    for collateral in borrower_collateral.collaterals.clone(){
        let collateral_token = collateral.0.clone();

        let price: PriceResponse = query_price(deps.as_ref(), config.anchor_oracle_contract.clone(), collateral_token,String::from("uusd"), Some(TimeConstraints{block_time: env.block.time.seconds(), valid_timeframe: overseer_config.price_timeframe}))?;

        prices.push(price.rate);
    }

    let liquidation_amount: LiquidationAmountResponse = deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart{
        contract_addr: config.anchor_liquidation_contract.into(),
        msg: to_binary(&LiquidationQueryMsg::LiquidationAmount{
            borrow_amount: borrower_loan.loan_amount,
            borrow_limit: borrower_limit.borrow_limit,
            collaterals: borrower_collateral.collaterals, //vec![(String::from("asdf"), Uint256::zero())],
            collateral_prices: prices.clone(), //vec![Decimal256::one()], //todo: need to call oracle, and parse prices
        })?,
    }))?;

    //calculate liquidation value to properly incentivize liquidator
    let mut liquidation_value: Uint256 = Uint256::zero();
    for collateral in liquidation_amount.collaterals.iter().zip(prices.iter()){
        let collateral_amount = collateral.0.1;
        let price = *collateral.1;

        liquidation_value += collateral_amount * price;
    }

    attrs.push(("liquidation_value", liquidation_value.to_string()));

    let liquidator_fee = liquidation_value * Decimal256::from(config.liquidator_fee);

    //calc UST value of guardians via astroport pools
    
    let ask_amount: Uint256 = liquidator_fee + borrower_loan.loan_amount - borrower_limit.borrow_limit;
    let ask_amount: Uint128 = ask_amount.into();
    let repayment_amount: Uint128 = (borrower_loan.loan_amount - borrower_limit.borrow_limit).into();
    let mut ask_amount_left: Uint128 = ask_amount + Uint128::from(GUARDIAN_BUFFER);

    attrs.push(("ask_amount_init", ask_amount.to_string()));
    attrs.push(("repayment_amount", repayment_amount.to_string()));

    //fetch guardians
    let borrower: Borrower = query_guardians(deps.as_ref(), address.clone())?;

    //execute swaps
    let mut messages = vec![];
    for guardian in borrower.guardians{

        let guardian_allowance: AllowanceResponse = deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart{
            contract_addr: guardian.address.clone().into(),
            msg: to_binary(&Cw20QueryMsg::Allowance{
                owner: address.clone().into(),
                spender: env.contract.address.clone().into(),
            })?
        }))?;

        let guardian_quantity_required: ReverseSimulationResponse = deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart{
            contract_addr: guardian.pair_address.clone().into(),
            msg: to_binary(&PairQueryMsg::ReverseSimulation{
                ask_asset: Asset{
                    info: AssetInfo::NativeToken{denom: String::from("uusd")},
                    amount: ask_amount_left,
                },
            })?,
        }))?;

        let guardian_offer_amount =  min(guardian_quantity_required.offer_amount, guardian_allowance.allowance);

        attrs.push(("guardian_offer_amount", guardian_offer_amount.to_string()));

        let expected_ust: SimulationResponse = deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart{
            contract_addr: guardian.pair_address.clone().into(),
            msg: to_binary(&PairQueryMsg::Simulation{
                offer_asset: Asset{
                    info: AssetInfo::Token{contract_addr: guardian.address.clone().into()},
                    amount: guardian_offer_amount,
                },
            })?
        }))?;

        //transfer in address's guardian, and swap to ust
        let transfer_from_msg = CosmosMsg::Wasm(WasmMsg::Execute{
            contract_addr: guardian.address.clone().into(),
            funds: vec![],
            msg: to_binary(&Cw20ExecuteMsg::TransferFrom{
                owner: address.clone().into(),
                recipient: env.contract.address.clone().into(),
                amount: guardian_offer_amount,
            })?
        });

        messages.push(transfer_from_msg);

        let swap_msg = CosmosMsg::Wasm(WasmMsg::Execute{
            contract_addr: guardian.address.clone().into(),
            funds: vec![],
            msg: to_binary(&Cw20ExecuteMsg::Send{
                contract: guardian.pair_address.clone().into(),
                amount: guardian_offer_amount,
                msg: to_binary(&PairHookMsg::Swap{
                    belief_price: None,
                    max_spread: None,
                    to: None,
                })?,
            })?
        });

        messages.push(swap_msg);

        attrs.push(("expected_ust", expected_ust.return_amount.to_string()));

        ask_amount_left = ask_amount_left - expected_ust.return_amount;

        attrs.push(("ask_amount_left", ask_amount_left.to_string()));

        if ask_amount <= Uint128::zero(){
            break;
        }
    }

    //if still in liquidation state, call normal anchor liquidation
    if ask_amount_left >= Uint128::from(1000000000u64){
        messages = vec![];

        messages.push(CosmosMsg::Wasm(WasmMsg::Execute{
            contract_addr: config.anchor_overseer_contract.into(),
            funds: vec![],
            msg: to_binary(&OverseerExecuteMsg::LiquidateCollateral{
                borrower: address.into(),
            })?
        }));
    } else {
        //cannot repay on behalf of another account
        //must use a smart contract wallet with proper exposed execute message
        
        messages.push(CosmosMsg::Wasm(WasmMsg::Execute{
            contract_addr: address.into(),
            funds: vec![
                Coin{
                    denom: String::from("uusd"),
                    amount: repayment_amount,
                }],
            msg: to_binary(&SmartWalletExecuteMsg::RepayStable{amount: repayment_amount})?,
        }));
        

        messages.push(CosmosMsg::Bank(BankMsg::Send{
            to_address: info.sender.into(),
            amount: vec![
                Coin{
                    denom: String::from("uusd"),
                    amount: liquidator_fee.into(),
                }
            ]
        }));
        
    }

    Ok(Response::new().add_attributes(attrs).add_messages(messages))
}


#[allow(clippy::too_many_arguments)]
pub fn execute_add_guardian(
    deps: DepsMut,
    info: MessageInfo,
    cw20_address: String,
    pair_address: String,
) -> StdResult<Response<TerraMsgWrapper>> {

    //confirm cw20 is whitelisted
    let state: State = STATE.load(deps.storage)?;
    let cw20_address = deps.api.addr_validate(&cw20_address)?;
    let pair_address = deps.api.addr_validate(&pair_address)?;

    if !state.whitelisted_cw20s.contains(&cw20_address){
        return Err(StdError::generic_err("Unauthorized"));
    }

    //update internal borrower guardian state
    let mut borrower: Borrower = BORROWERS.load(deps.storage, info.sender.clone()).unwrap_or(Borrower{
        guardians: vec![]
    });

    let new_guardian: Guardian = Guardian{
        address: cw20_address.clone(),
        pair_address: pair_address,
    };

    let guardian_position = borrower
        .guardians
        .iter()
        .position(|x| x.address == cw20_address);

    if let Some(guardian_position) = guardian_position{
        borrower.guardians.remove(guardian_position);
    }

    borrower.guardians.push(new_guardian);

    BORROWERS.save(deps.storage, info.sender, &borrower)?;

    Ok(Response::new().add_attributes(vec![("action", "add_guardian")]))
}


#[allow(clippy::too_many_arguments)]
pub fn execute_update_config(
    deps: DepsMut,
    info: MessageInfo,
    owner: String,
) -> StdResult<Response<TerraMsgWrapper>> {

    //priv check
    let mut config: Config = CONFIG.load(deps.storage)?;
    if config.owner != info.sender{
        return Err(StdError::generic_err("Unauthorized"));
    }

    //update config
    config.owner = deps.api.addr_validate(&owner)?;

    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attributes(vec![("action", "update_config")]))
}

#[allow(clippy::too_many_arguments)]
pub fn execute_whitelist_cw20(
    deps: DepsMut,
    info: MessageInfo,
    address: String,
) -> StdResult<Response<TerraMsgWrapper>> {

    //priv check
    let config: Config = CONFIG.load(deps.storage)?;
    if config.owner != info.sender{
        return Err(StdError::generic_err("Unauthorized"));
    }

    //valid address
    let cw20_address: Addr = deps.api.addr_validate(&address)?;

    //check if address already whitelisted
    let mut state: State = STATE.load(deps.storage)?;



    let cw20_address_check = state.whitelisted_cw20s.iter().any(|x| x == &cw20_address);

    if !cw20_address_check{
        state.whitelisted_cw20s.push(cw20_address);
        STATE.save(deps.storage, &state)?;
    }

    Ok(Response::new().add_attributes(vec![("action", "whitelist_cw20")]))
}


#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => Ok(to_binary(&query_config(deps)?)?),
        QueryMsg::Guardians {address} => Ok(to_binary(&query_guardians(deps, address)?)?),
    }
}


pub fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    let config: Config = CONFIG.load(deps.storage)?;
    Ok(ConfigResponse {
        owner: config.owner.into(),
        anchor_market_contract: config.anchor_market_contract.into(),
        anchor_overseer_contract: config.anchor_overseer_contract.into(),
        anchor_liquidation_contract: config.anchor_liquidation_contract.into(),
        anchor_oracle_contract: config.anchor_oracle_contract.into(),
        liquidator_fee: config.liquidator_fee,
    })
  }
  
  pub fn query_guardians(deps: Deps, address: String) -> StdResult<Borrower> {
    let borrower: Borrower = BORROWERS.load(deps.storage, deps.api.addr_validate(&address)?)?;
    Ok(borrower)
  }