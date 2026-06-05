use solana_program::pubkey::Pubkey;

use crate::error::ArcherAmmError;
use crate::state::{MakerBook, MarketStateHeader, PPM_DIVISOR};

#[derive(Debug, Clone, Copy)]
struct AggregatedLevel {
    price_ticks: u64,
    size_base_lots: u64,
    maker_index: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct QuoteOutput {
    pub out_amount: u64,
    pub fee_amount: u64,
}

pub fn compute_quote(
    input_amount: u64,
    is_buy: bool,
    header: &MarketStateHeader,
    maker_books: &[(Pubkey, MakerBook)],
    current_slot: u64,
    taker: Option<&Pubkey>,
) -> Result<QuoteOutput, ArcherAmmError> {
    if input_amount == 0 {
        return Ok(QuoteOutput {
            out_amount: 0,
            fee_amount: 0,
        });
    }

    if !header.is_active() {
        return Ok(QuoteOutput {
            out_amount: 0,
            fee_amount: 0,
        });
    }

    if header.is_async_only() {
        return Err(ArcherAmmError::AsyncNotSupported);
    }

    let effective_taker_fee_ppm = header.sync_taker_fee_ppm()?;

    let apply_sync_spread = header.is_hybrid();

    if !has_matching_liquidity(maker_books, is_buy, apply_sync_spread, current_slot, taker) {
        return Ok(QuoteOutput {
            out_amount: 0,
            fee_amount: 0,
        });
    }

    if is_buy {
        quote_buy_exact_in(
            input_amount,
            header,
            maker_books,
            effective_taker_fee_ppm,
            apply_sync_spread,
            current_slot,
            taker,
        )
    } else {
        quote_sell_exact_in(
            input_amount,
            header,
            maker_books,
            effective_taker_fee_ppm,
            apply_sync_spread,
            current_slot,
            taker,
        )
    }
}

fn quote_buy_exact_in(
    input_quote_atoms: u64,
    header: &MarketStateHeader,
    maker_books: &[(Pubkey, MakerBook)],
    effective_taker_fee_ppm: i32,
    apply_sync_spread: bool,
    current_slot: u64,
    taker: Option<&Pubkey>,
) -> Result<QuoteOutput, ArcherAmmError> {
    let input_quote_lots = input_quote_atoms / header.quote_atoms_per_quote_lot;

    if input_quote_lots == 0 {
        return Ok(QuoteOutput {
            out_amount: 0,
            fee_amount: 0,
        });
    }

    let matching_amount = if effective_taker_fee_ppm > 0 {
        let ppm = PPM_DIVISOR as u128;
        let fee = effective_taker_fee_ppm as u128;
        let denominator = ppm
            .checked_add(fee)
            .ok_or_else(|| ArcherAmmError::MathError("fee adjust overflow".into()))?;
        let adjusted = (input_quote_lots as u128)
            .checked_mul(ppm)
            .ok_or_else(|| ArcherAmmError::MathError("fee adjust overflow".into()))?
            .checked_div(denominator)
            .ok_or_else(|| ArcherAmmError::MathError("fee adjust div zero".into()))?;
        adjusted as u64
    } else {
        input_quote_lots
    };

    let mut all_asks =
        collect_all_levels(maker_books, false, apply_sync_spread, current_slot, taker);

    all_asks.sort_unstable_by(|a, b| {
        a.price_ticks
            .cmp(&b.price_ticks)
            .then(a.maker_index.cmp(&b.maker_index))
    });

    let mut remaining_quote_lots = matching_amount;
    let mut total_base_lots_out = 0u64;
    let mut total_quote_lots_matched = 0u64;

    let mut i = 0;
    while i < all_asks.len() && remaining_quote_lots > 0 {
        let price = all_asks[i].price_ticks;

        let group_end = {
            let mut j = i + 1;
            while j < all_asks.len() && all_asks[j].price_ticks == price {
                j += 1;
            }
            j
        };

        let total_size: u64 = all_asks[i..group_end]
            .iter()
            .map(|l| l.size_base_lots)
            .sum();

        let max_base = quote_to_base_lots(header, remaining_quote_lots, price, false)?;
        let base_to_fill = max_base.min(total_size);

        if base_to_fill == 0 {
            break;
        }

        let maker_levels = &all_asks[i..group_end];
        let num_makers = maker_levels.len();
        let mut filled_base = 0u64;
        let mut filled_quote = 0u64;
        let mut distributed = 0u64;

        for (idx, maker) in maker_levels.iter().enumerate() {
            let is_last = idx == num_makers - 1;

            let share = if is_last {
                base_to_fill
                    .saturating_sub(distributed)
                    .min(maker.size_base_lots)
            } else {
                calculate_pro_rata(base_to_fill, maker.size_base_lots, total_size)?
                    .min(maker.size_base_lots)
            };

            if share == 0 {
                continue;
            }

            let quote_cost = base_to_quote_lots(header, share, price, true)?;

            filled_base = filled_base
                .checked_add(share)
                .ok_or_else(|| ArcherAmmError::MathError("base overflow".into()))?;
            filled_quote = filled_quote
                .checked_add(quote_cost)
                .ok_or_else(|| ArcherAmmError::MathError("quote overflow".into()))?;
            distributed = distributed
                .checked_add(share)
                .ok_or_else(|| ArcherAmmError::MathError("base overflow".into()))?;
        }

        remaining_quote_lots = remaining_quote_lots
            .checked_sub(filled_quote)
            .ok_or_else(|| ArcherAmmError::MathError("remaining underflow".into()))?;
        total_quote_lots_matched = total_quote_lots_matched
            .checked_add(filled_quote)
            .ok_or_else(|| ArcherAmmError::MathError("quote overflow".into()))?;
        total_base_lots_out = total_base_lots_out
            .checked_add(filled_base)
            .ok_or_else(|| ArcherAmmError::MathError("base overflow".into()))?;

        i = group_end;
    }

    let taker_fee_lots = calculate_fee(total_quote_lots_matched, effective_taker_fee_ppm)?;

    let out_base_atoms = total_base_lots_out
        .checked_mul(header.base_atoms_per_base_lot)
        .ok_or_else(|| ArcherAmmError::MathError("base atoms overflow".into()))?;

    let fee_atoms = if taker_fee_lots >= 0 {
        (taker_fee_lots as u64)
            .checked_mul(header.quote_atoms_per_quote_lot)
            .ok_or_else(|| ArcherAmmError::MathError("fee atoms overflow".into()))?
    } else {
        taker_fee_lots
            .unsigned_abs()
            .checked_mul(header.quote_atoms_per_quote_lot)
            .ok_or_else(|| ArcherAmmError::MathError("fee atoms overflow".into()))?
    };

    Ok(QuoteOutput {
        out_amount: out_base_atoms,
        fee_amount: fee_atoms,
    })
}

fn quote_sell_exact_in(
    input_base_atoms: u64,
    header: &MarketStateHeader,
    maker_books: &[(Pubkey, MakerBook)],
    effective_taker_fee_ppm: i32,
    apply_sync_spread: bool,
    current_slot: u64,
    taker: Option<&Pubkey>,
) -> Result<QuoteOutput, ArcherAmmError> {
    let input_base_lots = input_base_atoms / header.base_atoms_per_base_lot;

    if input_base_lots == 0 {
        return Ok(QuoteOutput {
            out_amount: 0,
            fee_amount: 0,
        });
    }

    let mut all_bids =
        collect_all_levels(maker_books, true, apply_sync_spread, current_slot, taker);

    all_bids.sort_unstable_by(|a, b| {
        b.price_ticks
            .cmp(&a.price_ticks)
            .then(a.maker_index.cmp(&b.maker_index))
    });

    let mut remaining_base_lots = input_base_lots;
    let mut total_quote_lots_matched = 0u64;

    let mut i = 0;
    while i < all_bids.len() && remaining_base_lots > 0 {
        let price = all_bids[i].price_ticks;

        let group_end = {
            let mut j = i + 1;
            while j < all_bids.len() && all_bids[j].price_ticks == price {
                j += 1;
            }
            j
        };

        let total_size: u64 = all_bids[i..group_end]
            .iter()
            .map(|l| l.size_base_lots)
            .sum();

        let base_to_fill = remaining_base_lots.min(total_size);
        if base_to_fill == 0 {
            i = group_end;
            continue;
        }

        let maker_levels = &all_bids[i..group_end];
        let num_makers = maker_levels.len();
        let mut filled_base = 0u64;
        let mut distributed = 0u64;

        for (idx, maker) in maker_levels.iter().enumerate() {
            let is_last = idx == num_makers - 1;

            let share = if is_last {
                base_to_fill
                    .saturating_sub(distributed)
                    .min(maker.size_base_lots)
            } else {
                calculate_pro_rata(base_to_fill, maker.size_base_lots, total_size)?
                    .min(maker.size_base_lots)
            };

            if share == 0 {
                continue;
            }

            let quote_received = base_to_quote_lots(header, share, price, false)?;
            total_quote_lots_matched = total_quote_lots_matched
                .checked_add(quote_received)
                .ok_or_else(|| ArcherAmmError::MathError("quote overflow".into()))?;
            filled_base = filled_base
                .checked_add(share)
                .ok_or_else(|| ArcherAmmError::MathError("base overflow".into()))?;
            distributed = distributed
                .checked_add(share)
                .ok_or_else(|| ArcherAmmError::MathError("base overflow".into()))?;
        }

        remaining_base_lots = remaining_base_lots
            .checked_sub(filled_base)
            .ok_or_else(|| ArcherAmmError::MathError("remaining underflow".into()))?;

        i = group_end;
    }

    let taker_fee_lots = calculate_fee(total_quote_lots_matched, effective_taker_fee_ppm)?;

    let net_quote_lots = if taker_fee_lots >= 0 {
        total_quote_lots_matched
            .checked_sub(taker_fee_lots as u64)
            .ok_or_else(|| ArcherAmmError::MathError("fee exceeds output".into()))?
    } else {
        total_quote_lots_matched
            .checked_add(taker_fee_lots.unsigned_abs())
            .ok_or_else(|| ArcherAmmError::MathError("rebate overflow".into()))?
    };

    let out_quote_atoms = net_quote_lots
        .checked_mul(header.quote_atoms_per_quote_lot)
        .ok_or_else(|| ArcherAmmError::MathError("quote atoms overflow".into()))?;

    let fee_atoms = if taker_fee_lots >= 0 {
        (taker_fee_lots as u64)
            .checked_mul(header.quote_atoms_per_quote_lot)
            .ok_or_else(|| ArcherAmmError::MathError("fee atoms overflow".into()))?
    } else {
        taker_fee_lots
            .unsigned_abs()
            .checked_mul(header.quote_atoms_per_quote_lot)
            .ok_or_else(|| ArcherAmmError::MathError("fee atoms overflow".into()))?
    };

    Ok(QuoteOutput {
        out_amount: out_quote_atoms,
        fee_amount: fee_atoms,
    })
}

pub fn has_matching_liquidity(
    maker_books: &[(Pubkey, MakerBook)],
    is_buy: bool,
    apply_sync_spread: bool,
    current_slot: u64,
    taker: Option<&Pubkey>,
) -> bool {
    for (_, book) in maker_books {
        if !book.is_active() {
            continue;
        }
        if let Some(taker) = taker {
            if &book.maker == taker {
                continue;
            }
        }
        if book.is_stale(current_slot) {
            continue;
        }
        if apply_sync_spread && book.sync_spread_ticks == u16::MAX {
            continue;
        }
        let levels = if is_buy {
            &book.ask_levels
        } else {
            &book.bid_levels
        };
        for level in levels.iter() {
            if level.is_active() && level.absolute_price(book.mid_price_ticks).is_some() {
                return true;
            }
        }
    }
    false
}

fn collect_all_levels(
    maker_books: &[(Pubkey, MakerBook)],
    is_bid_side: bool,
    apply_sync_spread: bool,
    current_slot: u64,
    taker: Option<&Pubkey>,
) -> Vec<AggregatedLevel> {
    let mut levels = Vec::new();

    for (maker_idx, (_, book)) in maker_books.iter().enumerate() {
        if !book.is_active() {
            continue;
        }

        if let Some(taker) = taker {
            if &book.maker == taker {
                continue;
            }
        }

        if book.is_stale(current_slot) {
            continue;
        }

        let side_levels = if is_bid_side {
            &book.bid_levels
        } else {
            &book.ask_levels
        };

        let spread_offset = if apply_sync_spread {
            book.sync_spread_ticks
        } else {
            0
        };

        for level in side_levels.iter() {
            if !level.is_active() {
                continue;
            }

            let abs_price = match level.absolute_price(book.mid_price_ticks) {
                Some(p) => p,
                None => continue,
            };

            let effective_price = if spread_offset == 0 || spread_offset == u16::MAX {
                if spread_offset == u16::MAX {
                    continue;
                }
                abs_price
            } else if is_bid_side {
                match abs_price.checked_sub(spread_offset as u64) {
                    Some(p) if p > 0 => p,
                    _ => continue,
                }
            } else {
                match abs_price.checked_add(spread_offset as u64) {
                    Some(p) => p,
                    None => continue,
                }
            };

            levels.push(AggregatedLevel {
                price_ticks: effective_price,
                size_base_lots: level.size_in_base_lots,
                maker_index: maker_idx,
            });
        }
    }

    levels
}

fn base_to_quote_lots(
    market: &MarketStateHeader,
    base_lots: u64,
    price_ticks: u64,
    ceiling: bool,
) -> Result<u64, ArcherAmmError> {
    let quote_atoms = market.base_lots_to_quote_atoms(base_lots, price_ticks)?;

    let quote_atoms_u128 = quote_atoms as u128;
    let quote_atoms_per_lot = market.quote_atoms_per_quote_lot as u128;

    if quote_atoms_per_lot == 0 {
        return Err(ArcherAmmError::MathError("quote_atoms_per_lot is 0".into()));
    }

    let quote_lots = if ceiling {
        let adjustment = quote_atoms_per_lot
            .checked_sub(1)
            .ok_or_else(|| ArcherAmmError::MathError("adjustment underflow".into()))?;
        quote_atoms_u128
            .checked_add(adjustment)
            .ok_or_else(|| ArcherAmmError::MathError("ceiling overflow".into()))?
            .checked_div(quote_atoms_per_lot)
            .ok_or_else(|| ArcherAmmError::MathError("div zero".into()))?
    } else {
        quote_atoms_u128
            .checked_div(quote_atoms_per_lot)
            .ok_or_else(|| ArcherAmmError::MathError("div zero".into()))?
    };

    if quote_lots > u64::MAX as u128 {
        return Err(ArcherAmmError::MathError("quote lots overflow u64".into()));
    }

    Ok(quote_lots as u64)
}

fn quote_to_base_lots(
    market: &MarketStateHeader,
    quote_lots: u64,
    price_ticks: u64,
    round_up: bool,
) -> Result<u64, ArcherAmmError> {
    let base_atoms_per_base_unit = market.base_atoms_per_base_unit()?;

    let quote_atoms = (quote_lots as u128)
        .checked_mul(market.quote_atoms_per_quote_lot as u128)
        .ok_or_else(|| ArcherAmmError::MathError("overflow".into()))?;

    let numerator = quote_atoms
        .checked_mul(base_atoms_per_base_unit)
        .ok_or_else(|| ArcherAmmError::MathError("overflow".into()))?;

    let tick_size = market.tick_size_in_quote_atoms_per_base_unit as u128;
    let base_atoms_per_lot = market.base_atoms_per_base_lot as u128;

    let denominator = (price_ticks as u128)
        .checked_mul(tick_size)
        .ok_or_else(|| ArcherAmmError::MathError("overflow".into()))?
        .checked_mul(base_atoms_per_lot)
        .ok_or_else(|| ArcherAmmError::MathError("overflow".into()))?;

    if denominator == 0 {
        return Err(ArcherAmmError::MathError("denominator is 0".into()));
    }

    let base_lots = if round_up {
        let adjustment = denominator
            .checked_sub(1)
            .ok_or_else(|| ArcherAmmError::MathError("adjustment underflow".into()))?;
        numerator
            .checked_add(adjustment)
            .ok_or_else(|| ArcherAmmError::MathError("ceiling overflow".into()))?
            .checked_div(denominator)
            .ok_or_else(|| ArcherAmmError::MathError("div zero".into()))?
    } else {
        numerator
            .checked_div(denominator)
            .ok_or_else(|| ArcherAmmError::MathError("div zero".into()))?
    };

    if base_lots > u64::MAX as u128 {
        return Err(ArcherAmmError::MathError("base lots overflow u64".into()));
    }

    Ok(base_lots as u64)
}

fn calculate_pro_rata(
    total: u64,
    share: u64,
    total_shares: u64,
) -> Result<u64, ArcherAmmError> {
    if total_shares == 0 {
        return Err(ArcherAmmError::MathError("pro-rata div zero".into()));
    }
    let result = (total as u128)
        .checked_mul(share as u128)
        .ok_or_else(|| ArcherAmmError::MathError("pro-rata overflow".into()))?
        .checked_div(total_shares as u128)
        .ok_or_else(|| ArcherAmmError::MathError("pro-rata div zero".into()))?;
    Ok(result as u64)
}

fn calculate_fee(quote_lots: u64, fee_ppm: i32) -> Result<i64, ArcherAmmError> {
    let quote = quote_lots as i128;
    let fee_rate = fee_ppm as i128;
    let divisor = PPM_DIVISOR as i128;

    let fee_raw = quote
        .checked_mul(fee_rate)
        .ok_or_else(|| ArcherAmmError::MathError("fee multiply overflow".into()))?;

    let fee = if fee_raw > 0 {
        fee_raw
            .checked_add(
                divisor
                    .checked_sub(1)
                    .ok_or_else(|| ArcherAmmError::MathError("fee sub overflow".into()))?,
            )
            .ok_or_else(|| ArcherAmmError::MathError("fee add overflow".into()))?
            .checked_div(divisor)
            .ok_or_else(|| ArcherAmmError::MathError("fee div zero".into()))?
    } else if fee_raw < 0 {
        fee_raw
            .checked_div(divisor)
            .ok_or_else(|| ArcherAmmError::MathError("fee div zero".into()))?
    } else {
        0
    };

    i64::try_from(fee).map_err(|_| ArcherAmmError::MathError("fee overflow i64".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{MakerLevel, MAKER_STATUS_ACTIVE, MAX_LEVELS};

    #[test]
    fn test_calculate_fee_positive() {
        assert_eq!(calculate_fee(1000, 5000).unwrap(), 5);
        assert_eq!(calculate_fee(1001, 5000).unwrap(), 6);
        assert_eq!(calculate_fee(1, 1).unwrap(), 1);
    }

    #[test]
    fn test_calculate_fee_negative() {
        assert_eq!(calculate_fee(1000, -5000).unwrap(), -5);
        assert_eq!(calculate_fee(1001, -5000).unwrap(), -5);
    }

    #[test]
    fn test_calculate_fee_zero() {
        assert_eq!(calculate_fee(1000, 0).unwrap(), 0);
        assert_eq!(calculate_fee(0, 5000).unwrap(), 0);
    }

    fn empty_book(active: bool) -> MakerBook {
        MakerBook {
            discriminator: [0; 8],
            maker: Pubkey::default(),
            market: Pubkey::default(),
            delegate: Pubkey::default(),
            mid_price_ticks: 100,
            quote_delta_per_tick: 0,
            min_mid_price_ticks: 0,
            quote_locked: 0,
            quote_free: 0,
            base_locked: 0,
            base_free: 0,
            status: if active { MAKER_STATUS_ACTIVE } else { 2 },
            maker_book_bump: 0,
            sync_spread_ticks: 0,
            kind: 0,
            _status_padding: [0; 3],
            last_updated_sequence_number: 0,
            total_bid_base_lots: 0,
            tick_conversion_num: 0,
            tick_conversion_den: 0,
            bid_levels: [MakerLevel {
                size_in_base_lots: 0,
                price_offset_ticks: 0,
            }; MAX_LEVELS],
            ask_levels: [MakerLevel {
                size_in_base_lots: 0,
                price_offset_ticks: 0,
            }; MAX_LEVELS],
            last_updated_slot: 0,
            expiry_in_slots: 0,
            _reserved: [0; 6],
        }
    }

    #[test]
    fn test_has_matching_liquidity_detects_active_with_levels() {
        let mut book = empty_book(true);
        book.ask_levels[0] = MakerLevel {
            size_in_base_lots: 10,
            price_offset_ticks: 5,
        };
        let books = vec![(Pubkey::new_unique(), book)];
        assert!(has_matching_liquidity(&books, true, false, 0, None));
        assert!(!has_matching_liquidity(&books, false, false, 0, None));
    }

    #[test]
    fn test_has_matching_liquidity_skips_stale_book() {
        let mut book = empty_book(true);
        book.ask_levels[0] = MakerLevel {
            size_in_base_lots: 10,
            price_offset_ticks: 5,
        };
        book.last_updated_slot = 100;
        book.expiry_in_slots = 50;
        let books = vec![(Pubkey::new_unique(), book)];

        // Within the expiry window — still liquid.
        assert!(has_matching_liquidity(&books, true, false, 149, None));
        // Exactly at expiry — stale.
        assert!(!has_matching_liquidity(&books, true, false, 150, None));
        // Well past expiry — stale.
        assert!(!has_matching_liquidity(&books, true, false, 10_000, None));
    }

    #[test]
    fn test_has_matching_liquidity_skips_self_trade() {
        let taker = Pubkey::new_unique();
        let mut book = empty_book(true);
        book.maker = taker;
        book.ask_levels[0] = MakerLevel {
            size_in_base_lots: 10,
            price_offset_ticks: 5,
        };
        let books = vec![(Pubkey::new_unique(), book)];

        assert!(has_matching_liquidity(&books, true, false, 0, None));
        assert!(!has_matching_liquidity(&books, true, false, 0, Some(&taker)));
        let other = Pubkey::new_unique();
        assert!(has_matching_liquidity(&books, true, false, 0, Some(&other)));
    }

    #[test]
    fn test_raw_base_units_conversion_matches_v1() {
        use bytemuck::Zeroable;

        fn header(raw: u64) -> MarketStateHeader {
            let mut h = MarketStateHeader::zeroed();
            h.base_decimals = 6;
            h.base_atoms_per_base_lot = 1_000_000;
            h.tick_size_in_quote_atoms_per_base_unit = 1_000_000;
            h.quote_atoms_per_quote_lot = 1;
            h.raw_base_units_per_base_unit = raw;
            h
        }

        let q1 = header(1).base_lots_to_quote_atoms(1, 1).unwrap();
        let q10 = header(10).base_lots_to_quote_atoms(1, 1).unwrap();
        assert_eq!(q1, 1_000_000);
        assert_eq!(q10, 100_000);

        assert_eq!(quote_to_base_lots(&header(1), q1, 1, false).unwrap(), 1);
        assert_eq!(quote_to_base_lots(&header(10), q10, 1, false).unwrap(), 1);
    }
}
