extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events as _, Ledger as _},
    token::StellarAssetClient,
    token::TokenClient,
    vec, TryIntoVal,
};

struct Setup<'a> {
    env: Env,
    escrow: EscrowClient<'a>,
    token: TokenClient<'a>,
    client: Address,
    freelancer: Address,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let client = Address::generate(&env);
    let freelancer = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    StellarAssetClient::new(&env, &sac.address()).mint(&client, &1_000);
    let id = env.register(Escrow, ());
    let escrow = EscrowClient::new(&env, &id);
    escrow.initialize(&admin);
    let token = TokenClient::new(&env, &sac.address());
    Setup {
        env,
        escrow,
        token,
        client,
        freelancer,
    }
}

#[test]
fn initialize_only_once() {
    let s = setup();
    let other = Address::generate(&s.env);
    assert_eq!(
        s.escrow.try_initialize(&other),
        Err(Ok(EscrowError::AlreadyInitialized))
    );
}

#[test]
fn full_happy_path() {
    let s = setup();
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 100i128, 200i128],
    );
    assert_eq!(id, 1);
    s.escrow.deposit_funds(&id, &300);
    assert_eq!(s.token.balance(&s.client), 700);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Funded);
    s.escrow.release_milestone(&id, &0);
    s.escrow.release_milestone(&id, &1);
    assert_eq!(s.token.balance(&s.freelancer), 300);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Completed);
    s.escrow.issue_reputation(&id, &5);
    let rep = s.escrow.get_reputation(&s.freelancer);
    assert_eq!(rep.completed_contracts, 1);
    assert_eq!(rep.total_rating, 5);
}

#[test]
fn rejects_overfunding_and_double_release() {
    let s = setup();
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 100i128],
    );
    assert_eq!(
        s.escrow.try_deposit_funds(&id, &101),
        Err(Ok(EscrowError::OverFunded))
    );
    s.escrow.deposit_funds(&id, &100);
    s.escrow.release_milestone(&id, &0);
    assert!(s.escrow.try_release_milestone(&id, &0).is_err());
}

#[test]
fn dispute_split_and_cancel() {
    let s = setup();
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 400i128],
    );
    s.escrow.deposit_funds(&id, &400);
    s.escrow.open_dispute(&id, &s.freelancer);
    s.escrow.resolve_dispute(&id, &2_500);
    assert_eq!(s.token.balance(&s.freelancer), 100);
    assert_eq!(s.token.balance(&s.client), 900);

    let id2 = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 50i128],
    );
    s.escrow.deposit_funds(&id2, &50);
    s.escrow.cancel_contract(&id2);
    assert_eq!(s.token.balance(&s.client), 900);
    assert_eq!(
        s.escrow.get_contract(&id2).status,
        ContractStatus::Cancelled
    );
}

#[test]
fn pause_blocks_mutations() {
    let s = setup();
    s.escrow.set_paused(&true);
    assert_eq!(
        s.escrow.try_create_contract(
            &s.client,
            &s.freelancer,
            &s.token.address,
            &vec![&s.env, 1i128]
        ),
        Err(Ok(EscrowError::Paused))
    );
}

#[test]
fn expired_dispute_boundary_split_and_event() {
    let s = setup();
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 401i128],
    );
    s.escrow.deposit_funds(&id, &401);
    s.escrow.open_dispute(&id, &s.client);

    s.env.ledger().set_timestamp(DISPUTE_TIMEOUT_SECS - 1);
    assert_eq!(
        s.escrow.try_settle_expired_dispute(&id, &s.freelancer),
        Err(Ok(EscrowError::DisputeNotExpired))
    );
    let outsider = Address::generate(&s.env);
    assert_eq!(
        s.escrow.try_settle_expired_dispute(&id, &outsider),
        Err(Ok(EscrowError::Unauthorized))
    );

    s.env.ledger().set_timestamp(DISPUTE_TIMEOUT_SECS);
    s.escrow.settle_expired_dispute(&id, &s.freelancer);
    let events = s.env.events().all();
    let (_, topics, data) = events.get(events.len() - 1).unwrap();
    let event_name: soroban_sdk::Symbol = topics.get(0).unwrap().try_into_val(&s.env).unwrap();
    let event_id: u64 = topics.get(1).unwrap().try_into_val(&s.env).unwrap();
    let event_amounts: (i128, i128) = data.try_into_val(&s.env).unwrap();
    assert_eq!(event_name, symbol_short!("disp_exp"));
    assert_eq!(event_id, id);
    assert_eq!(event_amounts, (200, 201));

    assert_eq!(s.token.balance(&s.freelancer), 200);
    assert_eq!(s.token.balance(&s.client), 800);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Resolved);
}

#[test]
fn admin_resolution_prevents_expired_settlement() {
    let s = setup();
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 100i128],
    );
    s.escrow.deposit_funds(&id, &100);
    s.escrow.open_dispute(&id, &s.client);
    s.escrow.resolve_dispute(&id, &5_000);
    s.env.ledger().set_timestamp(DISPUTE_TIMEOUT_SECS);
    assert_eq!(
        s.escrow.try_settle_expired_dispute(&id, &s.freelancer),
        Err(Ok(EscrowError::InvalidStatus))
    );
}

#[test]
fn expired_settlement_rejects_non_disputed_and_is_blocked_while_paused() {
    let s = setup();
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 100i128],
    );
    s.escrow.deposit_funds(&id, &100);
    s.env.ledger().set_timestamp(DISPUTE_TIMEOUT_SECS);
    assert_eq!(
        s.escrow.try_settle_expired_dispute(&id, &s.client),
        Err(Ok(EscrowError::InvalidStatus))
    );
    s.escrow.open_dispute(&id, &s.client);
    s.env.ledger().set_timestamp(DISPUTE_TIMEOUT_SECS * 2);
    s.escrow.set_paused(&true);
    assert_eq!(
        s.escrow.try_settle_expired_dispute(&id, &s.client),
        Err(Ok(EscrowError::Paused))
    );
    s.escrow.set_paused(&false);
    s.escrow.settle_expired_dispute(&id, &s.client);
}
