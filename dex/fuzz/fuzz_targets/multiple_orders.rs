#![no_main]

use std::cell::RefMut;
use std::cmp::max;
use std::collections::HashMap;
use std::mem::size_of;

use arbitrary::{Arbitrary, Unstructured};
use bumpalo::Bump;
use itertools::Itertools;
use lazy_static::lazy_static;
use libfuzzer_sys::fuzz_target;
use solana_sdk::account_info::AccountInfo;

use serum_dex::error::{DexError, DexErrorCode};
use serum_dex::instruction::{CancelOrderInstruction, MarketInstruction, NewOrderInstruction};
use serum_dex::matching::Side;
use serum_dex::state::{strip_header, MarketState, OpenOrders, ToAlignedBytes};
use serum_dex_fuzz::{
    get_token_account_balance, new_dex_owned_account_with_lamports, new_sol_account,
    new_token_account, process_instruction, setup_market, MarketAccounts, COIN_LOT_SIZE,
    PC_LOT_SIZE,
};

#[derive(Debug, Arbitrary, Clone)]
enum Action {
    PlaceOrder {
        owner_id: OwnerId,
        instruction: NewOrderInstruction,
    },
    CancelOrder {
        owner_id: OwnerId,
        slot: u8,
        by_client_id: bool,
    },
    MatchOrders(u16),
    ConsumeEvents(u16),
    SettleFunds(OwnerId),
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy, PartialOrd, Ord)]
struct OwnerId(u8);

impl Arbitrary for OwnerId {
    fn arbitrary(u: &mut Unstructured<'_>) -> arbitrary::Result<Self> {
        let i: u8 = u.arbitrary()?;
        Ok(OwnerId(i % 8))
    }

    fn size_hint(_: usize) -> (usize, Option<usize>) {
        (1, Some(1))
    }
}

struct Owner<'bump> {
    signer_account: AccountInfo<'bump>,
    orders_account: AccountInfo<'bump>,
    coin_account: AccountInfo<'bump>,
    pc_account: AccountInfo<'bump>,
}

const INITIAL_COIN_BALANCE: u64 = 1_000_000_000;
const INITIAL_PC_BALANCE: u64 = 3_000_000_000;

impl<'bump> Owner<'bump> {
    fn new(market_accounts: &MarketAccounts<'bump>, bump: &'bump Bump) -> Self {
        let signer_account = new_sol_account(10, &bump);
        let orders_account = new_dex_owned_account_with_lamports(
            size_of::<OpenOrders>(),
            10000000000,
            market_accounts.market.owner,
            &bump,
        );
        let coin_account = new_token_account(
            market_accounts.coin_mint.key,
            signer_account.key,
            INITIAL_COIN_BALANCE,
            &bump,
        );
        let pc_account = new_token_account(
            market_accounts.pc_mint.key,
            signer_account.key,
            INITIAL_PC_BALANCE,
            &bump,
        );
        Self {
            signer_account,
            orders_account,
            coin_account,
            pc_account,
        }
    }

    fn open_orders(&self) -> Option<RefMut<OpenOrders>> {
        let (orders, _) = strip_header::<OpenOrders, u8>(&self.orders_account, false).ok()?;
        Some(orders)
    }
}

lazy_static! {
    static ref VERBOSE: u32 = std::env::var("FUZZ_VERBOSE")
        .map(|s| s.parse())
        .ok()
        .transpose()
        .ok()
        .flatten()
        .unwrap_or(0);
}

fuzz_target!(|actions: Vec<Action>| { run_actions(actions) });

fn run_actions(actions: Vec<Action>) {
    if *VERBOSE >= 1 {
        println!("{:#?}", actions);
    }

    let bump = Bump::new();
    let market_accounts = setup_market(&bump);
    let mut owners: HashMap<OwnerId, Owner> = HashMap::new();

    let max_possible_coin_gained = get_max_possible_coin_gained(&actions);
    let max_possible_coin_spent = get_max_possible_coin_spent(&actions);
    let max_possible_pc_gained = get_max_possible_pc_gained(&actions);
    let max_possible_pc_spent = get_max_possible_pc_spent(&actions);

    for action in actions {
        run_action(action, &market_accounts, &mut owners, &bump);
        if *VERBOSE >= 4 {
            run_action(
                Action::MatchOrders(100),
                &market_accounts,
                &mut owners,
                &bump,
            );
            run_action(
                Action::ConsumeEvents(100),
                &market_accounts,
                &mut owners,
                &bump,
            );
        }
    }

    let mut actions = Vec::new();
    for (owner_id, owner) in owners.iter().sorted_by_key(|(order_id, _)| *order_id) {
        if let Some(orders) = owner.open_orders() {
            for (slot, order_id) in orders.orders.iter().enumerate() {
                if *order_id > 0 {
                    if actions.len() % 8 == 0 {
                        actions.push(Action::MatchOrders(100));
                        actions.push(Action::ConsumeEvents(100));
                    }
                    actions.push(Action::CancelOrder {
                        owner_id: *owner_id,
                        slot: slot as u8,
                        by_client_id: false,
                    });
                }
            }
        }
    }
    actions.push(Action::MatchOrders(100));
    actions.push(Action::ConsumeEvents(100));
    for (owner_id, owner) in owners.iter().sorted_by_key(|(order_id, _)| *order_id) {
        if owner.open_orders().is_some() {
            actions.push(Action::SettleFunds(*owner_id));
        }
    }

    for action in actions {
        run_action(action, &market_accounts, &mut owners, &bump);
    }

    let market_state =
        MarketState::load(&market_accounts.market, market_accounts.market.owner).unwrap();
    let total_coin_bal: u64 = owners
        .values()
        .map(|owner| get_token_account_balance(&owner.coin_account))
        .sum();
    let total_pc_bal: u64 = owners
        .values()
        .map(|owner| get_token_account_balance(&owner.pc_account))
        .sum();
    assert_eq!(
        total_coin_bal + market_state.coin_fees_accrued,
        owners.len() as u64 * INITIAL_COIN_BALANCE
    );
    assert_eq!(
        total_pc_bal + market_state.pc_fees_accrued,
        owners.len() as u64 * INITIAL_PC_BALANCE
    );

    for (owner_id, owner) in &owners {
        let coin_bal = get_token_account_balance(&owner.coin_account);
        let pc_bal = get_token_account_balance(&owner.pc_account);

        if coin_bal > INITIAL_COIN_BALANCE {
            let gained = coin_bal - INITIAL_COIN_BALANCE;
            let bound = max_possible_coin_gained.get(owner_id).copied().unwrap_or(0);
            assert!(
                gained <= bound,
                "{:?} gained too much {} > {}",
                owner_id,
                gained,
                bound
            );
        }
        if pc_bal > INITIAL_PC_BALANCE {
            let gained = pc_bal - INITIAL_PC_BALANCE;
            let bound = max_possible_pc_gained.get(owner_id).copied().unwrap_or(0);
            assert!(
                gained <= bound,
                "{:?} gained too much {} > {}",
                owner_id,
                gained,
                bound
            );
        }
        if coin_bal < INITIAL_COIN_BALANCE {
            let spent = INITIAL_COIN_BALANCE - coin_bal;
            let bound = max_possible_coin_spent.get(owner_id).copied().unwrap_or(0);
            assert!(
                spent <= bound,
                "{:?} lost too much {} > {}",
                owner_id,
                spent,
                bound
            );
        }
        if pc_bal < INITIAL_PC_BALANCE {
            let spent = INITIAL_PC_BALANCE - pc_bal;
            let bound = max_possible_pc_spent.get(owner_id).copied().unwrap_or(0);
            assert!(
                spent <= bound,
                "{:?} lost too much {} > {}",
                owner_id,
                spent,
                bound
            );
        }

        owner
            .open_orders()
            .map(|orders| assert_eq!(orders.native_coin_total, 0));
        owner
            .open_orders()
            .map(|orders| assert_eq!(orders.native_pc_total, 0));
    }
}

fn run_action<'bump>(
    action: Action,
    market_accounts: &MarketAccounts<'bump>,
    owners: &mut HashMap<OwnerId, Owner<'bump>>,
    bump: &'bump Bump,
) {
    if *VERBOSE >= 2 {
        println!("{:?}", action);
    }

    match action {
        Action::PlaceOrder {
            owner_id,
            instruction,
        } => {
            let owner = owners
                .entry(owner_id)
                .or_insert_with(|| Owner::new(&market_accounts, &bump));

            process_instruction(
                market_accounts.market.owner,
                &[
                    market_accounts.market.clone(),
                    owner.orders_account.clone(),
                    market_accounts.req_q.clone(),
                    if instruction.side == Side::Bid {
                        owner.pc_account.clone()
                    } else {
                        owner.coin_account.clone()
                    },
                    owner.signer_account.clone(),
                    market_accounts.coin_vault.clone(),
                    market_accounts.pc_vault.clone(),
                    market_accounts.spl_token_program.clone(),
                    market_accounts.rent_sysvar.clone(),
                ],
                &MarketInstruction::NewOrder(instruction.clone()).pack(),
            )
            .map_err(|e| match e {
                DexError::ErrorCode(DexErrorCode::InsufficientFunds) => {}
                DexError::ErrorCode(DexErrorCode::RequestQueueFull) => {}
                e => Err(e).unwrap(),
            })
            .ok();
        }

        Action::CancelOrder {
            owner_id,
            slot,
            by_client_id,
        } => {
            if slot >= 128 {
                return;
            }
            let owner = match owners.get(&owner_id) {
                Some(owner) => owner,
                None => {
                    return;
                }
            };
            let (side, order_id, client_order_id) = {
                if let Some(orders) = owner.open_orders() {
                    if let Some(side) = orders.slot_side(slot) {
                        (
                            side,
                            orders.orders[slot as usize],
                            orders.client_order_ids[slot as usize],
                        )
                    } else {
                        return;
                    }
                } else {
                    return;
                }
            };

            let expects_zero_id = client_order_id == 0 && by_client_id;

            let instruction = if by_client_id {
                if client_order_id == 0 {
                    return;
                }
                MarketInstruction::CancelOrderByClientId(client_order_id)
            } else {
                MarketInstruction::CancelOrder(CancelOrderInstruction {
                    side,
                    order_id,
                    owner: [0u64; 4],
                    owner_slot: slot,
                })
            };
            process_instruction(
                market_accounts.market.owner,
                &[
                    market_accounts.market.clone(),
                    owner.orders_account.clone(),
                    market_accounts.req_q.clone(),
                    owner.signer_account.clone(),
                ],
                &instruction.pack(),
            )
            .map_err(|e| match e {
                DexError::ErrorCode(DexErrorCode::RequestQueueFull) => {}
                DexError::ErrorCode(DexErrorCode::ClientOrderIdIsZero) if expects_zero_id => {}
                e => Err(e).unwrap(),
            })
            .map(|_| {
                if expects_zero_id {
                    panic!(
                        "Should have gotten client cancel rejected for zero client id of {}",
                        client_order_id
                    )
                }
            })
            .ok();
        }

        Action::MatchOrders(limit) => process_instruction(
            market_accounts.market.owner,
            &[
                market_accounts.market.clone(),
                market_accounts.req_q.clone(),
                market_accounts.event_q.clone(),
                market_accounts.bids.clone(),
                market_accounts.asks.clone(),
                market_accounts.coin_vault.clone(),
                market_accounts.pc_vault.clone(),
            ],
            &MarketInstruction::MatchOrders(limit).pack(),
        )
        .unwrap(),

        Action::ConsumeEvents(limit) => {
            let mut accounts: Vec<AccountInfo> = owners
                .values()
                .filter(|owner| owner.open_orders().is_some())
                .map(|owner| owner.orders_account.clone())
                .sorted_by_key(|account_info| account_info.key.to_aligned_bytes())
                .collect();
            if accounts.is_empty() {
                return;
            }
            accounts.extend_from_slice(&[
                market_accounts.market.clone(),
                market_accounts.event_q.clone(),
                market_accounts.coin_vault.clone(),
                market_accounts.pc_vault.clone(),
            ]);
            process_instruction(
                market_accounts.market.owner,
                &accounts,
                &MarketInstruction::ConsumeEvents(limit).pack(),
            )
            .unwrap();
        }

        Action::SettleFunds(owner_id) => {
            let owner = match owners.get(&owner_id) {
                Some(owner) => owner,
                None => {
                    return;
                }
            };
            if !owner.open_orders().is_some() {
                return;
            }
            process_instruction(
                market_accounts.market.owner,
                &[
                    market_accounts.market.clone(),
                    owner.orders_account.clone(),
                    owner.signer_account.clone(),
                    market_accounts.coin_vault.clone(),
                    market_accounts.pc_vault.clone(),
                    owner.coin_account.clone(),
                    owner.pc_account.clone(),
                    market_accounts.vault_signer.clone(),
                    market_accounts.spl_token_program.clone(),
                ],
                &MarketInstruction::SettleFunds.pack(),
            )
            .unwrap();
        }
    };

    if *VERBOSE >= 2 {
        let total_free: u64 = owners
            .values()
            .filter_map(|owner| owner.open_orders())
            .map(|orders| orders.native_coin_free)
            .sum();
        let total_free_and_locked: u64 = owners
            .values()
            .filter_map(|owner| owner.open_orders())
            .map(|orders| orders.native_coin_total)
            .sum();
        let total_balances: u64 = owners
            .values()
            .map(|owner| get_token_account_balance(&owner.coin_account))
            .sum();
        let fees = MarketState::load(&market_accounts.market, market_accounts.market.owner)
            .unwrap()
            .coin_fees_accrued;
        println!(
            "{} {} {} {} {}",
            total_free,
            total_free_and_locked - total_free,
            total_balances,
            fees,
            total_free_and_locked + total_balances + fees,
        );
    }
    if *VERBOSE >= 3 {
        market_accounts.print_requests();
        market_accounts.print_events();
    }
}

fn get_max_possible_coin_gained(actions: &Vec<Action>) -> HashMap<OwnerId, u64> {
    let mut max_possible = HashMap::new();
    for action in actions {
        if let Action::PlaceOrder {
            owner_id,
            instruction,
        } = action
        {
            if instruction.side == Side::Bid {
                let value = max_possible.entry(*owner_id).or_insert(0u64);
                *value =
                    value.saturating_add(instruction.max_qty.get().saturating_mul(COIN_LOT_SIZE));
            }
        }
    }
    max_possible
}

fn get_max_possible_pc_spent(actions: &Vec<Action>) -> HashMap<OwnerId, u64> {
    let mut max_possible = HashMap::new();
    for action in actions {
        if let Action::PlaceOrder {
            owner_id,
            instruction,
        } = action
        {
            if instruction.side == Side::Bid {
                let cost = instruction
                    .max_qty
                    .get()
                    .saturating_mul(instruction.limit_price.get())
                    .saturating_mul(PC_LOT_SIZE);
                let cost_plus_fees = cost.saturating_add(cost / 100);
                let value = max_possible.entry(*owner_id).or_insert(0u64);
                *value = value.saturating_add(cost_plus_fees);
            }
        }
    }
    max_possible
}

fn get_max_possible_coin_spent(actions: &Vec<Action>) -> HashMap<OwnerId, u64> {
    let mut max_possible = HashMap::new();
    for action in actions {
        if let Action::PlaceOrder {
            owner_id,
            instruction,
        } = action
        {
            if instruction.side == Side::Ask {
                let value = max_possible.entry(*owner_id).or_insert(0u64);
                *value =
                    value.saturating_add(instruction.max_qty.get().saturating_mul(COIN_LOT_SIZE));
            }
        }
    }
    max_possible
}

fn get_max_possible_pc_gained(actions: &Vec<Action>) -> HashMap<OwnerId, u64> {
    let mut max_price = 0u64;
    let mut max_possible = HashMap::new();
    for action in actions {
        if let Action::PlaceOrder {
            owner_id,
            instruction,
        } = action
        {
            if instruction.side == Side::Bid {
                max_price = max(max_price, instruction.limit_price.get());
            }
            if instruction.side == Side::Ask {
                let max_take = instruction
                    .max_qty
                    .get()
                    .saturating_mul(max_price)
                    .saturating_mul(PC_LOT_SIZE);
                let max_provide = instruction
                    .max_qty
                    .get()
                    .saturating_mul(instruction.limit_price.get())
                    .saturating_mul(PC_LOT_SIZE);
                let max_provide_plus_rebate = max_provide.saturating_add(max_provide / 1000);
                let value = max_possible.entry(*owner_id).or_insert(0u64);
                *value = value.saturating_add(max(max_take, max_provide_plus_rebate));
            }
        }
    }
    max_possible
}
