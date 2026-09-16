use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};

use archer_jup::quote::compute_quote;
use archer_jup::{ArcherAmm, ARCHER_PROGRAM_ID};
use archer_sdk::onchain::{
    ArcherUnit,
    BaseAtomsPerLot, BaseLots, MakerBook, MakerLevel, MakerRegistry, MarketStateHeader,
    QuoteAtomsPerBaseUnitPerTick, QuoteAtomsPerLot, QuoteLots, MAKER_BOOK_DISCRIMINATOR,
    MAKER_REGISTRY_DISCRIMINATOR, MARKET_STATE_DISCRIMINATOR, MAX_LEVELS,
};
use bytemuck::Zeroable;
use jupiter_amm_interface::{Amm, AmmContext, FeeMode, KeyedAccount, QuoteParams, SwapMode, SwapParams};
use rand::{rngs::StdRng, Rng, SeedableRng};
use solana_pubkey::Pubkey;
use solana_sdk::account::Account;

/// Values that sit on arithmetic edges.
fn edge_u64(rng: &mut StdRng) -> u64 {
    match rng.random_range(0..10) {
        0 => 0,
        1 => 1,
        2 => u64::MAX,
        3 => u64::MAX - 1,
        4 => u64::MAX / 2,
        5 => 1 << 63,
        6 => (1u64 << 32) - 1,
        7 => 1 << 32,
        8 => rng.random_range(0..1_000_000),
        _ => rng.random(),
    }
}

fn edge_i64(rng: &mut StdRng) -> i64 {
    match rng.random_range(0..8) {
        0 => 0,
        1 => i64::MAX,
        2 => i64::MIN,
        3 => -1,
        4 => 1,
        5 => rng.random_range(-1_000_000..1_000_000),
        _ => rng.random(),
    }
}

fn random_header(rng: &mut StdRng) -> MarketStateHeader {
    let mut h = MarketStateHeader::zeroed();
    h.discriminator = *MARKET_STATE_DISCRIMINATOR;
    h.base_mint = Pubkey::new_unique();
    h.quote_mint = Pubkey::new_unique();
    h.base_vault = Pubkey::new_unique();
    h.quote_vault = Pubkey::new_unique();
    h.base_decimals = rng.random_range(0..=40);
    h.quote_decimals = rng.random_range(0..=40);
    h.base_atoms_per_base_lot = BaseAtomsPerLot::new(edge_u64(rng));
    h.quote_atoms_per_quote_lot = QuoteAtomsPerLot::new(edge_u64(rng));
    h.tick_size_in_quote_atoms_per_base_unit = QuoteAtomsPerBaseUnitPerTick::new(edge_u64(rng));
    h.raw_base_units_per_base_unit = edge_u64(rng);
    h.maker_fee_ppm = if rng.random_bool(0.5) { rng.random_range(-50_000..=100_000) } else { rng.random() };
    h.taker_fee_ppm = if rng.random_bool(0.5) { rng.random_range(0..=100_000) } else { rng.random() };
    h.status = rng.random_range(0..5);
    h
}

fn random_book(rng: &mut StdRng, market: Pubkey) -> MakerBook {
    let mut b = MakerBook::zeroed();
    b.discriminator = *MAKER_BOOK_DISCRIMINATOR;
    b.market = if rng.random_bool(0.9) { market } else { Pubkey::new_unique() };
    b.maker = Pubkey::new_unique();
    b.status = rng.random_range(0..4);
    b.kind = rng.random_range(0..3);
    b.mid_price_ticks = edge_u64(rng);
    b.mid_at_last_sync = if rng.random_bool(0.5) { b.mid_price_ticks } else { edge_u64(rng) };
    b.tick_conversion_num = edge_u64(rng);
    b.tick_conversion_den = edge_u64(rng);
    b.quote_locked = QuoteLots::new(edge_u64(rng));
    b.quote_free = QuoteLots::new(edge_u64(rng));
    b.base_locked = BaseLots::new(edge_u64(rng));
    b.base_free = BaseLots::new(edge_u64(rng));
    b.last_updated_slot = edge_u64(rng);
    b.expiry_in_slots = edge_u64(rng);
    for i in 0..MAX_LEVELS {
        if rng.random_bool(0.6) {
            b.bid_levels[i] = MakerLevel::new(BaseLots::new(edge_u64(rng)), edge_i64(rng));
        }
        if rng.random_bool(0.6) {
            b.ask_levels[i] = MakerLevel::new(BaseLots::new(edge_u64(rng)), edge_i64(rng));
        }
    }
    b
}

#[test]
fn engine_never_panics_on_adversarial_state() {
    let mut rng = StdRng::seed_from_u64(0xA5A5);
    for _ in 0..3_000 {
        let header = random_header(&mut rng);
        let market = Pubkey::new_unique();
        let n = rng.random_range(0..=70usize);
        let books: Vec<(Pubkey, MakerBook)> =
            (0..n).map(|_| (Pubkey::new_unique(), random_book(&mut rng, market))).collect();
        let amount = edge_u64(&mut rng);
        let slot = edge_u64(&mut rng);
        let is_buy = rng.random_bool(0.5);
        let taker = if rng.random_bool(0.3) { Some(books.first().map(|(_, b)| b.maker).unwrap_or_default()) } else { None };
        let r = catch_unwind(AssertUnwindSafe(|| {
            compute_quote(amount, is_buy, &header, &books, slot, taker.as_ref())
        }));
        assert!(r.is_ok(), "engine panicked: buy={is_buy} amount={amount} books={n}");
    }
}

#[test]
fn amm_lifecycle_never_panics_on_garbage_accounts() {
    let mut rng = StdRng::seed_from_u64(0x5A5A);
    let ctx = AmmContext { clock_ref: Default::default() };
    for round in 0..600 {
        let market = Pubkey::new_unique();
        let header = random_header(&mut rng);
        let mut market_data = bytemuck::bytes_of(&header).to_vec();
        // Sometimes truncate or corrupt the market account.
        match rng.random_range(0..6) {
            0 => market_data.truncate(rng.random_range(0..market_data.len())),
            1 => {
                let i = rng.random_range(8..market_data.len());
                market_data[i] = rng.random();
            }
            _ => {}
        }
        let keyed = KeyedAccount {
            key: market,
            account: Account { lamports: 1, data: market_data.clone(), owner: ARCHER_PROGRAM_ID, executable: false, rent_epoch: 0 },
            params: None,
        };
        let Ok(mut amm) = ArcherAmm::from_keyed_account(&keyed, &ctx) else { continue };

        let (registry_key, _) = Pubkey::find_program_address(&[b"maker_registry", market.as_ref()], &ARCHER_PROGRAM_ID);
        let mut reg = MakerRegistry::zeroed();
        reg.discriminator = *MAKER_REGISTRY_DISCRIMINATOR;
        reg.market = market;
        // Corrupt counts past the array, duplicates, and dead keys are all fair.
        reg.num_makers = rng.random();
        let n = rng.random_range(0..reg.makers.len());
        let mut book_keys = Vec::new();
        for i in 0..n {
            let k = if i > 0 && rng.random_bool(0.1) { reg.makers[i - 1] } else { Pubkey::new_unique() };
            reg.makers[i] = k;
            book_keys.push(k);
        }
        let mut map: HashMap<Pubkey, Account> = HashMap::new();
        map.insert(market, keyed.account.clone());
        map.insert(registry_key, Account { lamports: 1, data: bytemuck::bytes_of(&reg).to_vec(), owner: ARCHER_PROGRAM_ID, executable: false, rent_epoch: 0 });
        // Mints: sometimes plain, sometimes garbage of odd lengths.
        for mint in [header.base_mint, header.quote_mint] {
            let len = match rng.random_range(0..4) { 0 => 82, 1 => 0, 2 => rng.random_range(0..400), _ => 165 };
            let data: Vec<u8> = (0..len).map(|_| rng.random()).collect();
            map.insert(mint, Account { lamports: 1, data, owner: spl_token::ID, executable: false, rent_epoch: 0 });
        }
        for k in &book_keys {
            if rng.random_bool(0.2) {
                continue; // missing account
            }
            let mut data = bytemuck::bytes_of(&random_book(&mut rng, market)).to_vec();
            if rng.random_bool(0.15) {
                data.truncate(rng.random_range(0..data.len()));
            }
            map.insert(*k, Account { lamports: 1, data, owner: ARCHER_PROGRAM_ID, executable: false, rent_epoch: 0 });
        }

        let r = catch_unwind(AssertUnwindSafe(|| {
            for _ in 0..2 {
                let _ = amm.update(map.clone());
            }
            let _ = amm.get_reserve_mints();
            let _ = amm.get_accounts_to_update();
            let _ = amm.get_accounts_len();
            let _ = amm.is_active();
            for _ in 0..8 {
                let (input_mint, output_mint) = if rng.random_bool(0.5) {
                    (header.quote_mint, header.base_mint)
                } else {
                    (header.base_mint, header.quote_mint)
                };
                let _ = amm.quote(&QuoteParams {
                    amount: edge_u64(&mut rng),
                    input_mint,
                    output_mint,
                    swap_mode: if rng.random_bool(0.9) { SwapMode::ExactIn } else { SwapMode::ExactOut },
                    fee_mode: FeeMode::Normal,
                });
                let jup = Pubkey::default();
                let _ = amm.get_swap_and_account_metas(&SwapParams {
                    swap_mode: SwapMode::ExactIn,
                    in_amount: edge_u64(&mut rng),
                    out_amount: 0,
                    source_mint: input_mint,
                    destination_mint: output_mint,
                    source_token_account: Pubkey::new_unique(),
                    destination_token_account: Pubkey::new_unique(),
                    token_transfer_authority: Pubkey::new_unique(),
                    user: Pubkey::new_unique(),
                    payer: Pubkey::new_unique(),
                    quote_mint_to_referrer: None,
                    jupiter_program_id: &jup,
                    missing_dynamic_accounts_as_default: false,
                });
            }
        }));
        assert!(r.is_ok(), "adapter panicked in round {round}");
    }
}
