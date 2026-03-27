use solana_program::pubkey::Pubkey;

use crate::error::ArcherAmmError;
use crate::state::{MakerBook, MarketStateHeader, TakerOrderBook, PPM_DIVISOR};

#[derive(Debug, Clone, Copy)]
struct AggregatedLevel {
    price_ticks: u64,
    size_base_lots: u64,
    is_limit_order: bool,
    sequence_number: u64,
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
) -> Result<QuoteOutput, ArcherAmmError> {
    compute_quote_with_taker_book(input_amount, is_buy, header, maker_books, None)
}

pub fn compute_quote_with_taker_book(
    input_amount: u64,
    is_buy: bool,
    header: &MarketStateHeader,
    maker_books: &[(Pubkey, MakerBook)],
    taker_book: Option<&TakerOrderBook>,
) -> Result<QuoteOutput, ArcherAmmError> {
    if input_amount == 0 {
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

    if is_buy {
        quote_buy_exact_in(
            input_amount,
            header,
            maker_books,
            taker_book,
            effective_taker_fee_ppm,
            apply_sync_spread,
        )
    } else {
        quote_sell_exact_in(
            input_amount,
            header,
            maker_books,
            taker_book,
            effective_taker_fee_ppm,
            apply_sync_spread,
        )
    }
}

fn quote_buy_exact_in(
    input_quote_atoms: u64,
    header: &MarketStateHeader,
    maker_books: &[(Pubkey, MakerBook)],
    taker_book: Option<&TakerOrderBook>,
    effective_taker_fee_ppm: i32,
    apply_sync_spread: bool,
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

    let mut all_asks = collect_all_levels(maker_books, taker_book, false, apply_sync_spread);

    all_asks.sort_unstable_by(|a, b| {
        a.price_ticks
            .cmp(&b.price_ticks)
            .then(a.is_limit_order.cmp(&b.is_limit_order).reverse()) // true (LO) before false (maker)
            .then(a.sequence_number.cmp(&b.sequence_number))
    });

    let mut remaining_quote_lots = matching_amount;
    let mut total_base_lots_out = 0u64;
    let mut total_quote_lots_matched = 0u64;

    for level in &all_asks {
        if remaining_quote_lots == 0 {
            break;
        }

        let max_base = quote_to_base_lots(header, remaining_quote_lots, level.price_ticks, false)?;
        let fill_base = max_base.min(level.size_base_lots);

        if fill_base == 0 {
            break;
        }

        let quote_cost = base_to_quote_lots(header, fill_base, level.price_ticks, true)?;

        if quote_cost > remaining_quote_lots {
            break;
        }

        remaining_quote_lots = remaining_quote_lots
            .checked_sub(quote_cost)
            .ok_or_else(|| ArcherAmmError::MathError("remaining underflow".into()))?;
        total_quote_lots_matched = total_quote_lots_matched
            .checked_add(quote_cost)
            .ok_or_else(|| ArcherAmmError::MathError("quote overflow".into()))?;
        total_base_lots_out = total_base_lots_out
            .checked_add(fill_base)
            .ok_or_else(|| ArcherAmmError::MathError("base overflow".into()))?;
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
    taker_book: Option<&TakerOrderBook>,
    effective_taker_fee_ppm: i32,
    apply_sync_spread: bool,
) -> Result<QuoteOutput, ArcherAmmError> {
    let input_base_lots = input_base_atoms / header.base_atoms_per_base_lot;

    if input_base_lots == 0 {
        return Ok(QuoteOutput {
            out_amount: 0,
            fee_amount: 0,
        });
    }

    let mut all_bids = collect_all_levels(maker_books, taker_book, true, apply_sync_spread);

    all_bids.sort_unstable_by(|a, b| {
        b.price_ticks
            .cmp(&a.price_ticks)
            .then(a.is_limit_order.cmp(&b.is_limit_order).reverse()) // true (LO) before false (maker)
            .then(a.sequence_number.cmp(&b.sequence_number))
    });

    let mut remaining_base_lots = input_base_lots;
    let mut total_quote_lots_matched = 0u64;

    for level in &all_bids {
        if remaining_base_lots == 0 {
            break;
        }

        let fill_base = remaining_base_lots.min(level.size_base_lots);
        if fill_base == 0 {
            continue;
        }

        // Quote received for this fill: floor (Ask side)
        let quote_received = base_to_quote_lots(header, fill_base, level.price_ticks, false)?;

        remaining_base_lots = remaining_base_lots
            .checked_sub(fill_base)
            .ok_or_else(|| ArcherAmmError::MathError("remaining underflow".into()))?;
        total_quote_lots_matched = total_quote_lots_matched
            .checked_add(quote_received)
            .ok_or_else(|| ArcherAmmError::MathError("quote overflow".into()))?;
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

fn collect_all_levels(
    maker_books: &[(Pubkey, MakerBook)],
    taker_book: Option<&TakerOrderBook>,
    is_bid_side: bool,
    apply_sync_spread: bool,
) -> Vec<AggregatedLevel> {
    let mut levels = Vec::new();

    for (_, book) in maker_books {
        if !book.is_active() {
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
                is_limit_order: false,
                sequence_number: u64::MAX,
            });
        }
    }

    if let Some(tob) = taker_book {
        let orders = if is_bid_side {
            let count = tob.header.num_bids as usize;
            &tob.bid_orders[..count]
        } else {
            let count = tob.header.num_asks as usize;
            &tob.ask_orders[..count]
        };

        for order in orders {
            if !order.is_active() {
                continue;
            }
            if order.price_ticks == 0 {
                continue;
            }

            levels.push(AggregatedLevel {
                price_ticks: order.price_ticks,
                size_base_lots: order.remaining_base_lots,
                is_limit_order: true,
                sequence_number: order.sequence_number,
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
    let base_unit_atoms = 10u128
        .checked_pow(market.base_decimals as u32)
        .ok_or_else(|| ArcherAmmError::MathError("pow overflow".into()))?;

    let quote_atoms = (quote_lots as u128)
        .checked_mul(market.quote_atoms_per_quote_lot as u128)
        .ok_or_else(|| ArcherAmmError::MathError("overflow".into()))?;

    let numerator = quote_atoms
        .checked_mul(base_unit_atoms)
        .ok_or_else(|| ArcherAmmError::MathError("overflow".into()))?;

    let tick_size = market.tick_size_in_quote_atoms_per_base_unit as u128;
    let base_atoms_per_lot = market.base_atoms_per_base_lot as u128;
    let raw_base_units = market.raw_base_units_per_base_unit as u128;

    let denominator = (price_ticks as u128)
        .checked_mul(tick_size)
        .ok_or_else(|| ArcherAmmError::MathError("overflow".into()))?
        .checked_mul(base_atoms_per_lot)
        .ok_or_else(|| ArcherAmmError::MathError("overflow".into()))?
        .checked_mul(raw_base_units)
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
}
