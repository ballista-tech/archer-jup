pub mod error;
pub mod quote;
pub mod state;

use std::sync::atomic::Ordering;

use solana_program::instruction::AccountMeta;
use solana_pubkey::Pubkey;

use jupiter_amm_interface::{
    AccountProvider, Amm, AmmContext, AmmError, AmmLabel, ClockRef, KeyedAccount, Quote,
    QuoteParams, SingleProgramAmm, Swap, SwapAndAccountMetas, SwapMode, SwapParams,
};
use rust_decimal::Decimal;
use solana_account::ReadableAccount;

use crate::quote::{compute_quote, QuoteOutput};
use crate::state::{
    deserialize_maker_book, deserialize_market_header, deserialize_registry, MakerBook,
    MarketStateHeader, MARKET_DISCRIMINATOR,
};

pub const ARCHER_PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("Archer8kgiavM61GyusMzaaS2ft5sALtNsD1HxkUPMhy");

const SWAP_DISCRIMINATOR: u8 = 15;

const SPL_TOKEN_PROGRAM: Pubkey =
    solana_pubkey::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

const TOKEN_2022_PROGRAM: Pubkey =
    solana_pubkey::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

#[derive(Clone)]
pub struct ArcherAmm {
    pub market_key: Pubkey,
    pub registry_key: Pubkey,

    pub market_header: Option<MarketStateHeader>,
    pub maker_book_keys: Vec<Pubkey>,
    pub maker_books: Vec<(Pubkey, MakerBook)>,

    pub base_token_program: Pubkey,
    pub quote_token_program: Pubkey,

    base_mint_data: Vec<u8>,
    quote_mint_data: Vec<u8>,

    pub clock_ref: ClockRef,

    /// Quote token account that receives the integrator's share of taker fees.
    pub integrator_fee_wallet: Pubkey,
}

impl ArcherAmm {
    fn current_slot(&self) -> u64 {
        self.clock_ref.slot.load(Ordering::Relaxed)
    }

    fn current_epoch(&self) -> u64 {
        self.clock_ref.epoch.load(Ordering::Relaxed)
    }
}

impl SingleProgramAmm for ArcherAmm {
    const PROGRAM_ID: Pubkey = ARCHER_PROGRAM_ID;
    const LABEL: AmmLabel = "Archer";
}

impl Amm for ArcherAmm {
    fn from_keyed_account(
        keyed_account: &KeyedAccount,
        amm_context: &AmmContext,
    ) -> Result<Self, AmmError> {
        let market_key = keyed_account.key;
        let data = &keyed_account.account.data;

        if data.len() < 8 || &data[0..8] != MARKET_DISCRIMINATOR {
            return Err("Not an Archer market".into());
        }

        let (registry_key, _) = Pubkey::find_program_address(
            &[b"maker_registry", market_key.as_ref()],
            &ARCHER_PROGRAM_ID,
        );

        Ok(Self {
            market_key,
            registry_key,
            market_header: None,
            maker_book_keys: vec![],
            maker_books: vec![],
            base_token_program: SPL_TOKEN_PROGRAM,
            quote_token_program: SPL_TOKEN_PROGRAM,
            base_mint_data: vec![],
            quote_mint_data: vec![],
            clock_ref: amm_context.clock_ref.clone(),
            integrator_fee_wallet: Pubkey::default(),
        })
    }

    fn label(&self) -> AmmLabel {
        "Archer"
    }

    fn program_id(&self) -> Pubkey {
        ARCHER_PROGRAM_ID
    }

    fn key(&self) -> Pubkey {
        self.market_key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        match &self.market_header {
            Some(h) => vec![h.base_mint, h.quote_mint],
            None => vec![],
        }
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        let mut accounts = vec![self.market_key, self.registry_key];
        accounts.extend_from_slice(&self.maker_book_keys);
        if let Some(h) = &self.market_header {
            accounts.push(h.base_mint);
            accounts.push(h.quote_mint);
        }
        accounts
    }

    fn update(&mut self, account_provider: impl AccountProvider) -> Result<(), AmmError> {
        if let Some(market_account) = account_provider.get(&self.market_key) {
            let header = deserialize_market_header(market_account.data())
                .map_err(|e| AmmError::Custom(format!("Failed to deserialize market: {e}")))?;
            self.market_header = Some(header);
        }

        if let Some(header) = self.market_header {
            if let Some(base_mint_account) = account_provider.get(&header.base_mint) {
                self.base_token_program = detect_token_program_from_data(base_mint_account.data());
                self.base_mint_data = base_mint_account.data().to_vec();
            }
            if let Some(quote_mint_account) = account_provider.get(&header.quote_mint) {
                self.quote_token_program = detect_token_program_from_data(quote_mint_account.data());
                self.quote_mint_data = quote_mint_account.data().to_vec();
            }
        }

        if let Some(registry_account) = account_provider.get(&self.registry_key) {
            let data = registry_account.data();
            if data.len() >= state::MakerRegistry::LEN
                && &data[0..8] == state::REGISTRY_DISCRIMINATOR
            {
                let registry = deserialize_registry(data)
                    .map_err(|e| AmmError::Custom(format!("Failed to deserialize registry: {e}")))?;
                let num = registry.num_makers as usize;
                let mut deduped: Vec<Pubkey> = Vec::with_capacity(num);
                for key in &registry.makers[..num] {
                    if !deduped.contains(key) {
                        deduped.push(*key);
                    }
                }
                self.maker_book_keys = deduped;
            }
        }

        self.maker_books.clear();
        for book_key in self.maker_book_keys.clone() {
            if let Some(book_account) = account_provider.get(&book_key) {
                if let Ok(book) = deserialize_maker_book(book_account.data()) {
                    self.maker_books.push((book_key, book));
                }
            }
        }

        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote, AmmError> {
        let header = self
            .market_header
            .as_ref()
            .ok_or_else(|| AmmError::Custom("Market not loaded".into()))?;

        if !header.is_active() {
            return Err("Market not active".into());
        }

        if quote_params.swap_mode == SwapMode::ExactOut {
            return Err("ExactOut not supported".into());
        }

        let is_buy = quote_params.input_mint == header.quote_mint;
        let current_slot = self.current_slot();
        let current_epoch = self.current_epoch();

        let (input_mint_data, input_token_program, output_mint_data, output_token_program) =
            if is_buy {
                (
                    &self.quote_mint_data,
                    &self.quote_token_program,
                    &self.base_mint_data,
                    &self.base_token_program,
                )
            } else {
                (
                    &self.base_mint_data,
                    &self.base_token_program,
                    &self.quote_mint_data,
                    &self.quote_token_program,
                )
            };

        let input_transfer_fee = transfer_fee_atoms(
            input_mint_data,
            input_token_program,
            quote_params.amount,
            current_epoch,
        )?;
        let net_input = quote_params.amount.saturating_sub(input_transfer_fee);

        let QuoteOutput {
            out_amount,
            fee_amount,
        } = compute_quote(
            net_input,
            is_buy,
            header,
            &self.maker_books,
            current_slot,
            None,
        )
        .map_err(|e| AmmError::Custom(format!("{e}")))?;

        let output_transfer_fee =
            transfer_fee_atoms(output_mint_data, output_token_program, out_amount, current_epoch)?;
        let net_output = out_amount.saturating_sub(output_transfer_fee);

        let effective_fee_ppm = header.sync_taker_fee_ppm().unwrap_or(header.taker_fee_ppm);
        let fee_pct = if effective_fee_ppm != 0 {
            Decimal::new(effective_fee_ppm as i64, 6)
        } else {
            Decimal::ZERO
        };

        Ok(Quote {
            in_amount: quote_params.amount,
            out_amount: net_output,
            fee_amount,
            fee_mint: header.quote_mint,
            fee_pct,
        })
    }

    fn get_swap_and_account_metas(
        &self,
        swap_params: &SwapParams,
    ) -> Result<SwapAndAccountMetas, AmmError> {
        let header = self
            .market_header
            .as_ref()
            .ok_or_else(|| AmmError::Custom("Market not loaded".into()))?;

        let is_buy = swap_params.source_mint == header.quote_mint;
        let side: u8 = if is_buy { 0 } else { 1 }; // Bid=0, Ask=1

        let (taker_base_ata, taker_quote_ata) = if is_buy {
            (
                swap_params.destination_token_account,
                swap_params.source_token_account,
            )
        } else {
            (
                swap_params.source_token_account,
                swap_params.destination_token_account,
            )
        };

        let mut account_metas = vec![
            AccountMeta::new_readonly(swap_params.token_transfer_authority, true),
            AccountMeta::new(self.market_key, false),
            AccountMeta::new(self.integrator_fee_wallet, false),
            AccountMeta::new_readonly(header.base_mint, false),
            AccountMeta::new_readonly(header.quote_mint, false),
            AccountMeta::new(header.base_vault, false),
            AccountMeta::new(header.quote_vault, false),
            AccountMeta::new(taker_base_ata, false),
            AccountMeta::new(taker_quote_ata, false),
            AccountMeta::new_readonly(self.base_token_program, false),
            AccountMeta::new_readonly(self.quote_token_program, false),
        ];

        let current_slot = self.current_slot();
        for (book_key, book) in &self.maker_books {
            if book.is_active() && !book.is_stale(current_slot) {
                account_metas.push(AccountMeta::new(*book_key, false));
            }
        }

        let input_lots = if is_buy {
            swap_params
                .in_amount
                .checked_div(header.quote_atoms_per_quote_lot)
                .ok_or_else(|| AmmError::Custom("quote lot size is 0".into()))?
        } else {
            swap_params
                .in_amount
                .checked_div(header.base_atoms_per_base_lot)
                .ok_or_else(|| AmmError::Custom("base lot size is 0".into()))?
        };

        let mut data = Vec::with_capacity(19);
        data.push(SWAP_DISCRIMINATOR);
        data.extend_from_slice(&input_lots.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes()); // threshold = 0 (Jupiter handles slippage)
        data.push(side);
        data.push(0u8); // mode = MaxAmountIn (ExactIn)

        Ok(SwapAndAccountMetas {
            swap: Swap::Archer,
            account_metas,
        })
    }

    fn has_dynamic_accounts(&self) -> bool {
        true
    }

    fn requires_update_for_reserve_mints(&self) -> bool {
        true
    }

    fn supports_exact_out(&self) -> bool {
        false
    }

    fn is_active(&self) -> bool {
        self.market_header
            .as_ref()
            .map(|h| h.is_active())
            .unwrap_or(false)
    }

    fn get_accounts_len(&self) -> usize {
        // 11 fixed accounts + maker books
        11 + self.maker_books.len()
    }
}

fn detect_token_program_from_data(mint_data: &[u8]) -> Pubkey {
    if mint_data.len() > 82 {
        TOKEN_2022_PROGRAM
    } else {
        SPL_TOKEN_PROGRAM
    }
}

fn transfer_fee_atoms(
    mint_data: &[u8],
    token_program: &Pubkey,
    amount: u64,
    epoch: u64,
) -> Result<u64, AmmError> {
    if *token_program != TOKEN_2022_PROGRAM || mint_data.is_empty() {
        return Ok(0);
    }

    use spl_token_2022::extension::{
        transfer_fee::TransferFeeConfig, BaseStateWithExtensions, StateWithExtensions,
    };
    use spl_token_2022::state::Mint;

    let state = StateWithExtensions::<Mint>::unpack(mint_data)
        .map_err(|e| AmmError::Custom(format!("mint unpack: {e}")))?;

    let cfg = match state.get_extension::<TransferFeeConfig>() {
        Ok(cfg) => cfg,
        Err(_) => return Ok(0),
    };

    let fee = cfg
        .calculate_epoch_fee(epoch, amount)
        .ok_or_else(|| AmmError::Custom("transfer fee overflow".into()))?;

    Ok(fee)
}
