use solana_program::pubkey::Pubkey;

use crate::error::ArcherAmmError;
use archer_sdk::onchain::{ArcherUnit, BaseLots, MakerBook, MarketStateHeader, Ticks, PPM_DIVISOR};

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
    input_lots: u64,
    is_buy: bool,
    header: &MarketStateHeader,
    maker_books: &[(Pubkey, MakerBook)],
    current_slot: u64,
    taker: Option<&Pubkey>,
) -> Result<QuoteOutput, ArcherAmmError> {
    if input_lots == 0 {
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

    let effective_taker_fee_ppm = header.taker_fee_ppm;

    if !has_matching_liquidity(maker_books, is_buy, current_slot, taker, header.maker_fee_ppm) {
        return Ok(QuoteOutput {
            out_amount: 0,
            fee_amount: 0,
        });
    }

    if is_buy {
        quote_buy_exact_in(
            input_lots,
            header,
            maker_books,
            effective_taker_fee_ppm,
            current_slot,
            taker,
        )
    } else {
        quote_sell_exact_in(
            input_lots,
            header,
            maker_books,
            effective_taker_fee_ppm,
            current_slot,
            taker,
        )
    }
}

fn quote_buy_exact_in(
    input_quote_lots: u64,
    header: &MarketStateHeader,
    maker_books: &[(Pubkey, MakerBook)],
    effective_taker_fee_ppm: i32,
    current_slot: u64,
    taker: Option<&Pubkey>,
) -> Result<QuoteOutput, ArcherAmmError> {

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
        collect_all_levels(maker_books, false, current_slot, taker, header.maker_fee_ppm);

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
        let mut budget_exhausted = false;
        loop {
            let total_size = group_total(&all_asks[i..group_end])?;
            if total_size == 0 {
                break;
            }

            let max_base = quote_to_base_lots(header, remaining_quote_lots, price, false)?;
            let base_to_fill = max_base.min(total_size);
            if base_to_fill == 0 {
                budget_exhausted = true;
                break;
            }

            let (filled_base, filled_quote) = distribute(
                &mut all_asks[i..group_end],
                base_to_fill,
                total_size,
                |share| base_to_quote_lots(header, share, price, true),
            )?;
            if filled_base == 0 {
                break;
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
        }

        if budget_exhausted {
            break;
        }
        i = group_end;
    }

    let taker_fee_lots = calculate_fee(total_quote_lots_matched, effective_taker_fee_ppm)?;

    let out_base_atoms = total_base_lots_out
        .checked_mul(header.base_atoms_per_base_lot.as_u64())
        .ok_or_else(|| ArcherAmmError::MathError("base atoms overflow".into()))?;

    let fee_atoms = if taker_fee_lots >= 0 {
        (taker_fee_lots as u64)
            .checked_mul(header.quote_atoms_per_quote_lot.as_u64())
            .ok_or_else(|| ArcherAmmError::MathError("fee atoms overflow".into()))?
    } else {
        taker_fee_lots
            .unsigned_abs()
            .checked_mul(header.quote_atoms_per_quote_lot.as_u64())
            .ok_or_else(|| ArcherAmmError::MathError("fee atoms overflow".into()))?
    };

    Ok(QuoteOutput {
        out_amount: out_base_atoms,
        fee_amount: fee_atoms,
    })
}

fn quote_sell_exact_in(
    input_base_lots: u64,
    header: &MarketStateHeader,
    maker_books: &[(Pubkey, MakerBook)],
    effective_taker_fee_ppm: i32,
    current_slot: u64,
    taker: Option<&Pubkey>,
) -> Result<QuoteOutput, ArcherAmmError> {

    let mut all_bids =
        collect_all_levels(maker_books, true, current_slot, taker, header.maker_fee_ppm);

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

        loop {
            let total_size = group_total(&all_bids[i..group_end])?;
            if total_size == 0 {
                break;
            }

            let base_to_fill = remaining_base_lots.min(total_size);
            if base_to_fill == 0 {
                break;
            }

            let (filled_base, filled_quote) = distribute(
                &mut all_bids[i..group_end],
                base_to_fill,
                total_size,
                |share| base_to_quote_lots(header, share, price, false),
            )?;
            if filled_base == 0 {
                break;
            }

            remaining_base_lots = remaining_base_lots
                .checked_sub(filled_base)
                .ok_or_else(|| ArcherAmmError::MathError("remaining underflow".into()))?;
            total_quote_lots_matched = total_quote_lots_matched
                .checked_add(filled_quote)
                .ok_or_else(|| ArcherAmmError::MathError("quote overflow".into()))?;
        }

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
        .checked_mul(header.quote_atoms_per_quote_lot.as_u64())
        .ok_or_else(|| ArcherAmmError::MathError("quote atoms overflow".into()))?;

    let fee_atoms = if taker_fee_lots >= 0 {
        (taker_fee_lots as u64)
            .checked_mul(header.quote_atoms_per_quote_lot.as_u64())
            .ok_or_else(|| ArcherAmmError::MathError("fee atoms overflow".into()))?
    } else {
        taker_fee_lots
            .unsigned_abs()
            .checked_mul(header.quote_atoms_per_quote_lot.as_u64())
            .ok_or_else(|| ArcherAmmError::MathError("fee atoms overflow".into()))?
    };

    Ok(QuoteOutput {
        out_amount: out_quote_atoms,
        fee_amount: fee_atoms,
    })
}

fn group_total(group: &[AggregatedLevel]) -> Result<u64, ArcherAmmError> {
    group.iter().try_fold(0u64, |acc, l| {
        acc.checked_add(l.size_base_lots)
            .ok_or_else(|| ArcherAmmError::MathError("group size overflow".into()))
    })
}

fn distribute(
    group: &mut [AggregatedLevel],
    base_to_fill: u64,
    total_size: u64,
    quote_for: impl Fn(u64) -> Result<u64, ArcherAmmError>,
) -> Result<(u64, u64), ArcherAmmError> {
    let live = group.iter().filter(|l| l.size_base_lots > 0).count();
    let mut seen = 0usize;
    let mut distributed = 0u64;
    let mut filled_base = 0u64;
    let mut filled_quote = 0u64;

    for maker in group.iter_mut().filter(|l| l.size_base_lots > 0) {
        seen = seen
            .checked_add(1)
            .ok_or_else(|| ArcherAmmError::MathError("maker count overflow".into()))?;
        let is_last = seen == live;

        let share = if is_last {
            base_to_fill
                .checked_sub(distributed)
                .ok_or_else(|| ArcherAmmError::MathError("distributed underflow".into()))?
                .min(maker.size_base_lots)
        } else {
            calculate_pro_rata(base_to_fill, maker.size_base_lots, total_size)?
                .min(maker.size_base_lots)
        };
        if share == 0 {
            continue;
        }

        let quote = quote_for(share)?;
        maker.size_base_lots = maker
            .size_base_lots
            .checked_sub(share)
            .ok_or_else(|| ArcherAmmError::MathError("level underflow".into()))?;
        distributed = distributed
            .checked_add(share)
            .ok_or_else(|| ArcherAmmError::MathError("base overflow".into()))?;
        filled_base = filled_base
            .checked_add(share)
            .ok_or_else(|| ArcherAmmError::MathError("base overflow".into()))?;
        filled_quote = filled_quote
            .checked_add(quote)
            .ok_or_else(|| ArcherAmmError::MathError("quote overflow".into()))?;
    }

    if distributed > base_to_fill {
        return Err(ArcherAmmError::MathError("distributed more than requested".into()));
    }
    Ok((filled_base, filled_quote))
}

pub fn book_is_eligible(
    book: &MakerBook,
    current_slot: u64,
    taker: Option<&Pubkey>,
    maker_fee_ppm: i32,
) -> bool {
    if let Some(taker) = taker {
        if book.is_authorized(taker) {
            return false;
        }
    }
    book.get_status()
        .map(|s| s.can_participate_in_auction())
        .unwrap_or(false)
        && !book.is_stale(current_slot)
        && book.is_quote_sync_fundable(maker_fee_ppm)
}

pub fn has_matching_liquidity(
    maker_books: &[(Pubkey, MakerBook)],
    is_buy: bool,
    current_slot: u64,
    taker: Option<&Pubkey>,
    maker_fee_ppm: i32,
) -> bool {
    for (_, book) in maker_books {
        if !book_is_eligible(book, current_slot, taker, maker_fee_ppm) {
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
    current_slot: u64,
    taker: Option<&Pubkey>,
    maker_fee_ppm: i32,
) -> Vec<AggregatedLevel> {
    let mut levels = Vec::new();

    for (maker_idx, (_, book)) in maker_books.iter().enumerate() {
        if !book_is_eligible(book, current_slot, taker, maker_fee_ppm) {
            continue;
        }

        let side_levels = if is_bid_side {
            &book.bid_levels
        } else {
            &book.ask_levels
        };

        for level in side_levels.iter() {
            if !level.is_active() {
                continue;
            }

            let abs_price = match level.absolute_price(book.mid_price_ticks) {
                Some(p) => p,
                None => continue,
            };

            levels.push(AggregatedLevel {
                price_ticks: abs_price,
                size_base_lots: level.size_in_base_lots.as_u64(),
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
    let quote_atoms = market
        .base_lots_to_quote_atoms(BaseLots::new(base_lots), Ticks::new(price_ticks))
        .map_err(|e| ArcherAmmError::MathError(format!("{e:?}")))?;

    let quote_atoms_u128 = quote_atoms.as_u64() as u128;
    let quote_atoms_per_lot = market.quote_atoms_per_quote_lot.as_u64() as u128;

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
        .checked_mul(market.quote_atoms_per_quote_lot.as_u64() as u128)
        .ok_or_else(|| ArcherAmmError::MathError("overflow".into()))?;

    let numerator = quote_atoms
        .checked_mul(base_atoms_per_base_unit)
        .ok_or_else(|| ArcherAmmError::MathError("overflow".into()))?;

    let tick_size = market.tick_size_in_quote_atoms_per_base_unit.as_u64() as u128;
    let base_atoms_per_lot = market.base_atoms_per_base_lot.as_u64() as u128;

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
    use archer_sdk::onchain::{
        BaseAtomsPerLot, MakerBookStatus, MakerLevel, QuoteAtomsPerBaseUnitPerTick, QuoteAtomsPerLot,
        QuoteLots,
    };
    use bytemuck::Zeroable;

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

    /// 1:1 lot/tick market: 1 base lot at price p costs p quote lots.
    fn header(maker_fee_ppm: i32, taker_fee_ppm: i32) -> MarketStateHeader {
        let mut h = MarketStateHeader::zeroed();
        h.status = 0;
        h.base_decimals = 6;
        h.quote_decimals = 6;
        h.base_atoms_per_base_lot = BaseAtomsPerLot::new(1_000_000);
        h.quote_atoms_per_quote_lot = QuoteAtomsPerLot::new(1);
        h.tick_size_in_quote_atoms_per_base_unit = QuoteAtomsPerBaseUnitPerTick::new(1);
        h.raw_base_units_per_base_unit = 1;
        h.maker_fee_ppm = maker_fee_ppm;
        h.taker_fee_ppm = taker_fee_ppm;
        h
    }

    fn empty_book(active: bool) -> MakerBook {
        let mut b = MakerBook::zeroed();
        b.mid_price_ticks = 100;
        b.tick_conversion_num = 1;
        b.tick_conversion_den = 1;
        b.status = if active {
            MakerBookStatus::Active.as_u8()
        } else {
            MakerBookStatus::Suspended.as_u8()
        };
        b
    }

    /// A book that the program would accept: balances lock exactly what the
    /// resting levels need (1:1 market, `maker_fee_ppm` buffer on bids).
    fn book(mid: u64, bids: &[(u64, i64)], asks: &[(u64, i64)], maker_fee_ppm: i32) -> MakerBook {
        let mut b = empty_book(true);
        b.mid_price_ticks = mid;
        b.mid_at_last_sync = mid;
        let mut base = 0u64;
        for (i, (size, off)) in asks.iter().enumerate() {
            b.ask_levels[i] = MakerLevel::new(BaseLots::new(*size), *off);
            base += size;
        }
        for (i, (size, off)) in bids.iter().enumerate() {
            b.bid_levels[i] = MakerLevel::new(BaseLots::new(*size), *off);
        }
        b.base_locked = BaseLots::new(base);
        b.quote_locked = QuoteLots::new(b.required_quote_reserve(maker_fee_ppm).unwrap());
        b
    }

    #[test]
    fn test_has_matching_liquidity_detects_active_with_levels() {
        let mut book = empty_book(true);
        book.ask_levels[0] = MakerLevel::new(BaseLots::new(10), 5);
        let books = vec![(Pubkey::new_unique(), book)];
        assert!(has_matching_liquidity(&books, true, 0, None, 0));
        assert!(!has_matching_liquidity(&books, false, 0, None, 0));
    }

    #[test]
    fn test_has_matching_liquidity_skips_stale_book() {
        let mut book = empty_book(true);
        book.ask_levels[0] = MakerLevel::new(BaseLots::new(10), 5);
        book.last_updated_slot = 100;
        book.expiry_in_slots = 50;
        let books = vec![(Pubkey::new_unique(), book)];
        assert!(has_matching_liquidity(&books, true, 149, None, 0));
        assert!(!has_matching_liquidity(&books, true, 150, None, 0));
        assert!(!has_matching_liquidity(&books, true, 10_000, None, 0));
    }

    #[test]
    fn test_has_matching_liquidity_skips_self_trade_and_delegate() {
        let maker = Pubkey::new_unique();
        let delegate = Pubkey::new_unique();
        let mut book = empty_book(true);
        book.maker = maker;
        book.delegate = delegate;
        book.ask_levels[0] = MakerLevel::new(BaseLots::new(10), 5);
        let books = vec![(Pubkey::new_unique(), book)];
        assert!(has_matching_liquidity(&books, true, 0, None, 0));
        assert!(has_matching_liquidity(&books, true, 0, Some(&Pubkey::new_unique()), 0));
        assert!(!has_matching_liquidity(&books, true, 0, Some(&maker), 0));
        assert!(!has_matching_liquidity(&books, true, 0, Some(&delegate), 0));
        // A book with no delegate must not exclude a taker at the default key.
        book.delegate = Pubkey::default();
        let books = vec![(Pubkey::new_unique(), book)];
        assert!(has_matching_liquidity(&books, true, 0, Some(&Pubkey::default()), 0));
    }

    #[test]
    fn test_suspended_and_undecodable_status_are_ineligible() {
        let mut book = empty_book(false);
        book.ask_levels[0] = MakerLevel::new(BaseLots::new(10), 5);
        assert!(!book_is_eligible(&book, 0, None, 0));
        book.status = 0;
        assert!(!book_is_eligible(&book, 0, None, 0));
        book.status = 1;
        assert!(book_is_eligible(&book, 0, None, 0));
    }

    /// A maker who repriced further than their quote balance can back is
    /// dropped from the auction on-chain, so the quote must skip them too.
    #[test]
    fn test_unfundable_book_is_skipped() {
        let mut book = empty_book(true);
        book.ask_levels[0] = MakerLevel::new(BaseLots::new(10), 5);
        book.bid_levels[0] = MakerLevel::new(BaseLots::new(1_000), -100);
        book.mid_at_last_sync = 100_000;
        book.mid_price_ticks = 100_010;
        book.quote_locked = QuoteLots::new(99_900_000);
        book.quote_free = QuoteLots::new(10_000);
        assert!(!book.is_quote_sync_fundable(200));
        book.quote_free = QuoteLots::new(200_000);
        assert_eq!(
            book.projected_quote_balances(200).unwrap(),
            (99_910_000 + 19_982, 100_100_000 - 99_910_000 - 19_982)
        );
        book.quote_free = QuoteLots::new(10_000);
        let books = vec![(Pubkey::new_unique(), book)];
        assert!(!has_matching_liquidity(&books, true, 0, None, 200));
        assert!(!book_is_eligible(&book, 0, None, 200));
    }

    #[test]
    fn test_zero_anchor_is_always_fundable() {
        let mut book = empty_book(true);
        book.mid_at_last_sync = 0;
        book.mid_price_ticks = 500_000;
        book.bid_levels[0] = MakerLevel::new(BaseLots::new(u64::MAX / 2), -1);
        assert!(book.is_quote_sync_fundable(200));
        assert_eq!(book.projected_quote_balances(200).ok(), Some((0, 0)));
    }

    /// Three makers with one lot each at the same price, taker wants two:
    /// pro-rata floors every non-last share to zero, the last maker fills one
    /// lot, and the program comes back to the same price for the second lot.
    /// Moving to the next price after one pass would quote the wrong price.
    #[test]
    fn test_pro_rata_dust_revisits_the_same_price() {
        let h = header(0, 0);
        let books: Vec<_> = (0..3)
            .map(|_| (Pubkey::new_unique(), book(100, &[], &[(1, 5)], 0)))
            .collect();
        // One more maker further out that a fall-through would hit.
        let mut all = books.clone();
        all.push((Pubkey::new_unique(), book(100, &[], &[(5, 50)], 0)));

        let q = compute_quote(2 * 105, true, &h, &all, 0, None).unwrap();
        assert_eq!(q.out_amount, 2 * 1_000_000, "both lots fill at 105");
        // Exactly 210 quote lots spent: the second lot came from price 105,
        // not 150.
        let q3 = compute_quote(3 * 105, true, &h, &all, 0, None).unwrap();
        assert_eq!(q3.out_amount, 3 * 1_000_000);
        let q4 = compute_quote(3 * 105 + 149, true, &h, &all, 0, None).unwrap();
        assert_eq!(q4.out_amount, 3 * 1_000_000, "149 is not enough for a lot at 150");
        let q5 = compute_quote(3 * 105 + 150, true, &h, &all, 0, None).unwrap();
        assert_eq!(q5.out_amount, 4 * 1_000_000);
    }

    #[test]
    fn test_buy_grosses_down_by_taker_fee_and_sell_nets_it() {
        let h = header(0, 10_000); // 1% taker fee
        let books = vec![(Pubkey::new_unique(), book(100, &[(10, -5)], &[(10, 5)], 0))];
        // 1010 quote lots gross down to 1000 -> 9 lots at 105 (945), fee ceil(9.45)=10.
        let q = compute_quote(1_010, true, &h, &books, 0, None).unwrap();
        assert_eq!(q.out_amount, 9 * 1_000_000);
        assert_eq!(q.fee_amount, 10);
        // Sell 10 lots at 95 = 950, fee ceil(9.5) = 10, net 940.
        let q = compute_quote(10, false, &h, &books, 0, None).unwrap();
        assert_eq!(q.out_amount, 940);
        assert_eq!(q.fee_amount, 10);
    }

    #[test]
    fn test_raw_base_units_conversion_matches_v1() {
        fn header(raw: u64) -> MarketStateHeader {
            let mut h = MarketStateHeader::zeroed();
            h.base_decimals = 6;
            h.base_atoms_per_base_lot = BaseAtomsPerLot::new(1_000_000);
            h.tick_size_in_quote_atoms_per_base_unit = QuoteAtomsPerBaseUnitPerTick::new(1_000_000);
            h.quote_atoms_per_quote_lot = QuoteAtomsPerLot::new(1);
            h.raw_base_units_per_base_unit = raw;
            h
        }
        let q1 = header(1).base_lots_to_quote_atoms(BaseLots::new(1), Ticks::new(1)).unwrap();
        let q10 = header(10).base_lots_to_quote_atoms(BaseLots::new(1), Ticks::new(1)).unwrap();
        assert_eq!(q1.as_u64(), 1_000_000);
        assert_eq!(q10.as_u64(), 100_000);
        assert_eq!(quote_to_base_lots(&header(1), q1.as_u64(), 1, false).unwrap(), 1);
        assert_eq!(quote_to_base_lots(&header(10), q10.as_u64(), 1, false).unwrap(), 1);
    }

    #[test]
    fn layout_sizes_match_the_program() {
        assert_eq!(core::mem::size_of::<MakerBook>(), 776);
        assert_eq!(core::mem::size_of::<MarketStateHeader>(), 272);
    }
}
