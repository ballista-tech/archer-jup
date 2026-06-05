use bytemuck::{Pod, Zeroable};
use solana_program::pubkey::Pubkey;

use crate::error::ArcherAmmError;

pub const MARKET_DISCRIMINATOR: &[u8; 8] = b"ACHRMKT1";
pub const MAKER_BOOK_DISCRIMINATOR: &[u8; 8] = b"ACHRMKR1";
pub const REGISTRY_DISCRIMINATOR: &[u8; 8] = b"ACHRREG1";

pub const PPM_DIVISOR: u64 = 1_000_000;

pub const MARKET_STATUS_ACTIVE: u8 = 0;
pub const MARKET_MODE_CONTINUOUS: u8 = 0;
pub const MARKET_MODE_ASYNC: u8 = 1;
pub const MARKET_MODE_HYBRID: u8 = 2;

pub const MAKER_STATUS_ACTIVE: u8 = 1;

pub const MAKER_KIND_MM: u8 = 0;
pub const MAKER_KIND_LO: u8 = 1;

pub const MAX_LEVELS: usize = 16;
pub const MAX_MAKERS: usize = 64;

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct MarketStateHeader {
    pub discriminator: [u8; 8],
    pub market_id: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub admin: Pubkey,
    pub base_atoms_per_base_lot: u64,
    pub quote_atoms_per_quote_lot: u64,
    pub tick_size_in_quote_atoms_per_base_unit: u64,
    pub raw_base_units_per_base_unit: u64,
    pub uncollected_fees_quote_lots: u64,
    pub collected_fees_quote_lots: u64,
    pub maker_fee_ppm: i32,
    pub taker_fee_ppm: i32,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    pub status: u8,
    pub mode: u8,
    pub market_bump: u8,
    pub sync_fee_multiplier: u8,
    pub min_async_delay_slots: u16,
    pub max_async_delay_slots: u16,
    pub _reserved: u32,
}

unsafe impl Pod for MarketStateHeader {}
unsafe impl Zeroable for MarketStateHeader {}

impl MarketStateHeader {
    pub const LEN: usize = core::mem::size_of::<Self>();

    pub fn is_active(&self) -> bool {
        self.status == MARKET_STATUS_ACTIVE
    }

    pub fn is_hybrid(&self) -> bool {
        self.mode == MARKET_MODE_HYBRID
    }

    pub fn is_async_only(&self) -> bool {
        self.mode == MARKET_MODE_ASYNC
    }

    pub fn effective_sync_fee_multiplier(&self) -> u8 {
        if self.sync_fee_multiplier == 0 {
            1
        } else {
            self.sync_fee_multiplier
        }
    }

    pub fn sync_taker_fee_ppm(&self) -> Result<i32, ArcherAmmError> {
        if !self.is_hybrid() {
            return Ok(self.taker_fee_ppm);
        }
        let multiplier = self.effective_sync_fee_multiplier() as i32;
        self.taker_fee_ppm
            .checked_mul(multiplier)
            .ok_or_else(|| ArcherAmmError::MathError("sync fee overflow".into()))
    }

    pub fn base_atoms_per_base_unit(&self) -> Result<u128, ArcherAmmError> {
        10u128
            .checked_pow(self.base_decimals as u32)
            .ok_or_else(|| ArcherAmmError::MathError("pow overflow".into()))?
            .checked_mul(self.raw_base_units_per_base_unit as u128)
            .ok_or_else(|| ArcherAmmError::MathError("base unit overflow".into()))
    }

    pub fn base_lots_to_quote_atoms(
        &self,
        base_lots: u64,
        price_ticks: u64,
    ) -> Result<u64, ArcherAmmError> {
        let base_atoms = (base_lots as u128)
            .checked_mul(self.base_atoms_per_base_lot as u128)
            .ok_or_else(|| ArcherAmmError::MathError("overflow".into()))?;

        let base_atoms_per_base_unit = self.base_atoms_per_base_unit()?;

        let quote_atoms = base_atoms
            .checked_mul(price_ticks as u128)
            .ok_or_else(|| ArcherAmmError::MathError("overflow".into()))?
            .checked_mul(self.tick_size_in_quote_atoms_per_base_unit as u128)
            .ok_or_else(|| ArcherAmmError::MathError("overflow".into()))?
            .checked_div(base_atoms_per_base_unit)
            .ok_or_else(|| ArcherAmmError::MathError("div zero".into()))?;

        if quote_atoms > u64::MAX as u128 {
            return Err(ArcherAmmError::MathError("quote atoms overflow u64".into()));
        }

        Ok(quote_atoms as u64)
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct MakerLevel {
    pub size_in_base_lots: u64,
    pub price_offset_ticks: i64,
}

unsafe impl Pod for MakerLevel {}
unsafe impl Zeroable for MakerLevel {}

impl MakerLevel {
    pub fn is_active(&self) -> bool {
        self.size_in_base_lots > 0
    }

    pub fn absolute_price(&self, mid_price_ticks: u64) -> Option<u64> {
        let abs = (mid_price_ticks as i64).checked_add(self.price_offset_ticks)?;
        if abs <= 0 {
            None
        } else {
            Some(abs as u64)
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct MakerBook {
    pub discriminator: [u8; 8],
    pub maker: Pubkey,
    pub market: Pubkey,
    pub delegate: Pubkey,
    pub mid_price_ticks: u64,
    pub quote_delta_per_tick: u64,
    pub min_mid_price_ticks: u64,
    pub quote_locked: u64,
    pub quote_free: u64,
    pub base_locked: u64,
    pub base_free: u64,
    pub status: u8,
    pub maker_book_bump: u8,
    pub sync_spread_ticks: u16,
    pub kind: u8,
    pub _status_padding: [u8; 3],
    pub last_updated_sequence_number: u64,
    pub total_bid_base_lots: u64,
    pub tick_conversion_num: u64,
    pub tick_conversion_den: u64,
    pub bid_levels: [MakerLevel; MAX_LEVELS],
    pub ask_levels: [MakerLevel; MAX_LEVELS],
    pub last_updated_slot: u64,
    pub expiry_in_slots: u64,
    pub _reserved: [u64; 6],
}

unsafe impl Pod for MakerBook {}
unsafe impl Zeroable for MakerBook {}

impl MakerBook {
    pub const LEN: usize = core::mem::size_of::<Self>();

    pub fn is_active(&self) -> bool {
        self.status == MAKER_STATUS_ACTIVE
    }

    pub fn is_limit_order(&self) -> bool {
        self.kind == MAKER_KIND_LO
    }

    pub fn is_stale(&self, current_slot: u64) -> bool {
        self.expiry_in_slots > 0
            && current_slot.saturating_sub(self.last_updated_slot) >= self.expiry_in_slots
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct MakerRegistry {
    pub discriminator: [u8; 8],
    pub market: Pubkey,
    pub admin: Pubkey,
    pub num_makers: u8,
    pub _padding: [u8; 7],
    pub makers: [Pubkey; MAX_MAKERS],
}

unsafe impl Pod for MakerRegistry {}
unsafe impl Zeroable for MakerRegistry {}

impl MakerRegistry {
    pub const LEN: usize = core::mem::size_of::<Self>();
}

pub fn deserialize_market_header(data: &[u8]) -> Result<MarketStateHeader, ArcherAmmError> {
    if data.len() < MarketStateHeader::LEN {
        return Err(ArcherAmmError::DeserializationFailed(
            "Market data too short".into(),
        ));
    }
    if &data[0..8] != MARKET_DISCRIMINATOR {
        return Err(ArcherAmmError::DeserializationFailed(
            "Invalid market discriminator".into(),
        ));
    }
    let header = bytemuck::try_from_bytes::<MarketStateHeader>(&data[..MarketStateHeader::LEN])
        .map_err(|e| ArcherAmmError::DeserializationFailed(format!("Market: {e}")))?;
    Ok(*header)
}

pub fn deserialize_maker_book(data: &[u8]) -> Result<MakerBook, ArcherAmmError> {
    if data.len() < MakerBook::LEN {
        return Err(ArcherAmmError::DeserializationFailed(
            "MakerBook data too short".into(),
        ));
    }
    if &data[0..8] != MAKER_BOOK_DISCRIMINATOR {
        return Err(ArcherAmmError::DeserializationFailed(
            "Invalid maker book discriminator".into(),
        ));
    }
    let book = bytemuck::try_from_bytes::<MakerBook>(&data[..MakerBook::LEN])
        .map_err(|e| ArcherAmmError::DeserializationFailed(format!("MakerBook: {e}")))?;
    Ok(*book)
}

pub fn deserialize_registry(data: &[u8]) -> Result<MakerRegistry, ArcherAmmError> {
    if data.len() < MakerRegistry::LEN {
        return Err(ArcherAmmError::DeserializationFailed(
            "Registry data too short".into(),
        ));
    }
    if &data[0..8] != REGISTRY_DISCRIMINATOR {
        return Err(ArcherAmmError::DeserializationFailed(
            "Invalid registry discriminator".into(),
        ));
    }
    let registry = bytemuck::try_from_bytes::<MakerRegistry>(&data[..MakerRegistry::LEN])
        .map_err(|e| ArcherAmmError::DeserializationFailed(format!("Registry: {e}")))?;
    Ok(*registry)
}
