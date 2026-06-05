#[cfg(test)]
mod simulations {
    use litesvm::LiteSVM;
    use rand::Rng;

    use solana_client::nonblocking::rpc_client::RpcClient;
    use solana_compute_budget::compute_budget::ComputeBudget;
    use solana_program::native_token::LAMPORTS_PER_SOL;
    use solana_program::program_pack::Pack;
    use solana_pubkey::Pubkey;
    use solana_sdk::account::{Account, ReadableAccount, WritableAccount};
    use solana_sdk::signature::Keypair;
    use solana_sdk::signer::Signer;
    use solana_sdk::transaction::Transaction;
    use solana_sysvar::clock::{self, Clock};
    use std::collections::HashMap;
    use std::str::FromStr;
    use std::time::Instant;

    use spl_associated_token_account::get_associated_token_address_with_program_id;
    use spl_token::state::{Account as TokenAccount, AccountState};

    use std::env;

    use jupiter_amm_interface::{
        Amm, AmmContext, FeeMode, KeyedAccount, QuoteParams, SwapMode, SwapParams,
    };

    use archer_jup::ArcherAmm;
    use archer_jup::ARCHER_PROGRAM_ID;

    /// A `HashMap<Pubkey, Account>` satisfies Jupiter's `AccountProvider`.
    type AccountMap = HashMap<Pubkey, Account>;

    fn init_test_logger() {
        let _ = dotenvy::dotenv();
        let _ = env_logger::builder().is_test(true).try_init();
    }

    /// Fetch accounts from RPC and build an AccountMap.
    async fn fetch_account_map(rpc: &RpcClient, keys: &[Pubkey]) -> AccountMap {
        let mut map = AccountMap::default();
        for key in keys {
            if let Ok(account) = rpc.get_account(key).await {
                map.insert(*key, account);
            }
        }
        map
    }

    /// Build an AccountMap from LiteSVM state.
    fn svm_account_map(svm: &LiteSVM, keys: &[Pubkey]) -> AccountMap {
        let mut map = AccountMap::default();
        for key in keys {
            if let Some(acc) = svm.get_account(key) {
                map.insert(*key, acc);
            }
        }
        map
    }

    /// Fetch many accounts from RPC in one `getMultipleAccounts` call (blocking).
    ///
    /// Atomic per chunk: every account in a chunk is read at the same slot, so the
    /// snapshot is internally consistent (book sizes match vault balances). This is
    /// what makes the boundary/random sim cross-checks deterministic against a live,
    /// actively-updated market — sequential single-account reads race the makers.
    fn fetch_multiple_blocking(rpc: &RpcClient, keys: &[Pubkey]) -> Vec<(Pubkey, Account)> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut out = Vec::new();
                for chunk in keys.chunks(100) {
                    if let Ok(accounts) = rpc.get_multiple_accounts(chunk).await {
                        for (pk, maybe) in chunk.iter().zip(accounts) {
                            if let Some(acc) = maybe {
                                out.push((*pk, acc));
                            }
                        }
                    }
                }
                out
            })
        })
    }

    fn compute_bounds(amm: &ArcherAmm, input_mint: &Pubkey, output_mint: &Pubkey) -> (u64, u64) {
        let header = amm.market_header.as_ref().unwrap();
        let is_buy = *input_mint == header.quote_mint;

        let min_lot = if is_buy {
            header.quote_atoms_per_quote_lot
        } else {
            header.base_atoms_per_base_lot
        };

        let quote_output = |amount: u64| -> u64 {
            amm.quote(&QuoteParams {
                amount,
                input_mint: *input_mint,
                output_mint: *output_mint,
                swap_mode: SwapMode::ExactIn,
                fee_mode: FeeMode::Normal,
            })
            .map(|r| r.out_amount)
            .unwrap_or(0)
        };

        let max_output = quote_output(u64::MAX / 2);
        if max_output == 0 {
            return (min_lot, min_lot);
        }

        let mut lo = min_lot;
        let mut hi = u64::MAX / 2;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if quote_output(mid) > 0 {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        let lower_bound = lo;

        lo = lower_bound;
        hi = u64::MAX / 2;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if quote_output(mid) >= max_output {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        let upper_bound = lo;

        (lower_bound, upper_bound)
    }

    pub fn setup_litesvm() -> (LiteSVM, Keypair) {
        let mut litesvm = LiteSVM::new()
            .with_compute_budget(ComputeBudget {
                compute_unit_limit: 1_400_000,
                ..Default::default()
            })
            .with_blockhash_check(false)
            .with_sigverify(false)
            .with_transaction_history(0);

        let program_path = "programs/archer_v1.so".to_string();
        litesvm
            .add_program_from_file(ARCHER_PROGRAM_ID, &program_path)
            .unwrap_or_else(|e| {
                panic!(
                    "Failed to load Archer program from {}: {}. \
                     Place the Archer program binary at this path to run simulation tests.",
                    program_path, e
                )
            });

        let keypair = Keypair::new();
        let account = Account {
            lamports: 10_000 * LAMPORTS_PER_SOL,
            data: vec![],
            owner: solana_sdk::system_program::id(),
            executable: false,
            rent_epoch: 0,
        };
        litesvm.set_account(keypair.pubkey(), account).unwrap();

        (litesvm, keypair)
    }

    /// Load all accounts needed for simulation into LiteSVM from RPC,
    /// then re-initialize the AMM from the frozen LiteSVM state.
    fn snapshot_state_into_svm(
        amm: &mut ArcherAmm,
        rpc: &RpcClient,
        litesvm: &mut LiteSVM,
        keypair: &Keypair,
    ) {
        let mints = amm.get_reserve_mints();
        let base_mint = mints[0];
        let quote_mint = mints[1];

        let base_token_program = amm.base_token_program;
        let quote_token_program = amm.quote_token_program;

        let dummy_ata_base = get_associated_token_address_with_program_id(
            &keypair.pubkey(),
            &base_mint,
            &base_token_program,
        );
        let dummy_ata_quote = get_associated_token_address_with_program_id(
            &keypair.pubkey(),
            &quote_mint,
            &quote_token_program,
        );

        let jup_program = Pubkey::default();

        // Build swap instructions for both directions to discover all needed accounts
        let swap_params_buy = SwapParams {
            swap_mode: SwapMode::ExactIn,
            in_amount: 1,
            out_amount: 0,
            source_mint: quote_mint,
            destination_mint: base_mint,
            source_token_account: dummy_ata_quote,
            destination_token_account: dummy_ata_base,
            token_transfer_authority: keypair.pubkey(),
            user: keypair.pubkey(),
            payer: keypair.pubkey(),
            quote_mint_to_referrer: None,
            jupiter_program_id: &jup_program,
            missing_dynamic_accounts_as_default: false,
        };

        let swap_params_sell = SwapParams {
            swap_mode: SwapMode::ExactIn,
            in_amount: 1,
            out_amount: 0,
            source_mint: base_mint,
            destination_mint: quote_mint,
            source_token_account: dummy_ata_base,
            destination_token_account: dummy_ata_quote,
            token_transfer_authority: keypair.pubkey(),
            user: keypair.pubkey(),
            payer: keypair.pubkey(),
            quote_mint_to_referrer: None,
            jupiter_program_id: &jup_program,
            missing_dynamic_accounts_as_default: false,
        };

        let ix_buy = amm.get_swap_and_account_metas(&swap_params_buy).unwrap();
        let ix_sell = amm.get_swap_and_account_metas(&swap_params_sell).unwrap();

        // Collect all unique account pubkeys (excluding user ATAs)
        let mut all_pks: Vec<Pubkey> = vec![amm.registry_key];
        for sam in [&ix_buy, &ix_sell] {
            for acc in &sam.account_metas {
                if acc.pubkey != dummy_ata_base
                    && acc.pubkey != dummy_ata_quote
                    && acc.pubkey != keypair.pubkey()
                    && !all_pks.contains(&acc.pubkey)
                {
                    all_pks.push(acc.pubkey);
                }
            }
        }

        // Also include mint accounts for token program detection
        let header = amm.market_header.as_ref().unwrap();
        if !all_pks.contains(&header.base_mint) {
            all_pks.push(header.base_mint);
        }
        if !all_pks.contains(&header.quote_mint) {
            all_pks.push(header.quote_mint);
        }

        // Load all accounts from RPC into LiteSVM atomically (single slot).
        for (pk, acc) in fetch_multiple_blocking(rpc, &all_pks) {
            if acc.executable {
                continue;
            }
            litesvm.set_account(pk, acc).unwrap();
        }

        // Create a synthetic integrator fee wallet for testing purposes
        let integrator_fee_wallet = Keypair::new();
        let mut fee_account = Account::new(LAMPORTS_PER_SOL, TokenAccount::LEN, &quote_token_program);
        let mut fee_token_data = TokenAccount::default();
        fee_token_data.mint = quote_mint;
        fee_token_data.owner = keypair.pubkey();
        fee_token_data.state = AccountState::Initialized;
        fee_token_data.amount = 0;
        fee_token_data.pack_into_slice(fee_account.data_as_mut_slice());
        litesvm
            .set_account(integrator_fee_wallet.pubkey(), fee_account)
            .unwrap();
        amm.integrator_fee_wallet = integrator_fee_wallet.pubkey();

        // Re-initialize AMM from LiteSVM's frozen state
        let accounts_to_update = amm.get_accounts_to_update();
        let account_map = svm_account_map(litesvm, &accounts_to_update);
        amm.update(account_map).unwrap();

        // Second pass to pick up dynamic accounts
        let accounts_to_update = amm.get_accounts_to_update();
        let account_map = svm_account_map(litesvm, &accounts_to_update);
        amm.update(account_map).unwrap();
    }

    /// Simulate a swap and return the output amount.
    /// Uses simulate_transaction so LiteSVM state is NOT modified.
    fn sim_quote_request(
        amm: &ArcherAmm,
        input_mint: Pubkey,
        input_amount: u64,
        output_mint: Pubkey,
        litesvm: &mut LiteSVM,
        keypair: &Keypair,
    ) -> u64 {
        let mints = amm.get_reserve_mints();
        let base_mint = mints[0];

        let header = amm.market_header.as_ref().unwrap();
        let (input_token_program, output_token_program) = if input_mint == base_mint {
            (amm.base_token_program, amm.quote_token_program)
        } else {
            (amm.quote_token_program, amm.base_token_program)
        };

        let source_ata = get_associated_token_address_with_program_id(
            &keypair.pubkey(),
            &input_mint,
            &input_token_program,
        );
        let dest_ata = get_associated_token_address_with_program_id(
            &keypair.pubkey(),
            &output_mint,
            &output_token_program,
        );

        let jup_program = Pubkey::default();

        let swap_params = SwapParams {
            swap_mode: SwapMode::ExactIn,
            in_amount: input_amount,
            out_amount: 0,
            source_mint: input_mint,
            destination_mint: output_mint,
            source_token_account: source_ata,
            destination_token_account: dest_ata,
            token_transfer_authority: keypair.pubkey(),
            user: keypair.pubkey(),
            payer: keypair.pubkey(),
            quote_mint_to_referrer: None,
            jupiter_program_id: &jup_program,
            missing_dynamic_accounts_as_default: false,
        };

        let swap_result = amm.get_swap_and_account_metas(&swap_params).unwrap();

        // Build the instruction manually from the account metas
        let ix = solana_program::instruction::Instruction {
            program_id: ARCHER_PROGRAM_ID,
            accounts: swap_result.account_metas,
            data: {
                let is_buy = input_mint == header.quote_mint;
                let side: u8 = if is_buy { 0 } else { 1 };
                let input_lots = if is_buy {
                    input_amount / header.quote_atoms_per_quote_lot
                } else {
                    input_amount / header.base_atoms_per_base_lot
                };
                let mut data = Vec::with_capacity(19);
                data.push(15u8); // SWAP_DISCRIMINATOR
                data.extend_from_slice(&input_lots.to_le_bytes());
                data.extend_from_slice(&0u64.to_le_bytes());
                data.push(side);
                data.push(0u8);
                data
            },
        };

        // Create synthetic token accounts
        let mut account_a = Account::new(LAMPORTS_PER_SOL, TokenAccount::LEN, &input_token_program);
        let mut account_a_data = TokenAccount::default();
        account_a_data.mint = input_mint;
        account_a_data.owner = keypair.pubkey();
        account_a_data.state = AccountState::Initialized;
        account_a_data.amount = u64::MAX;
        account_a_data.pack_into_slice(account_a.data_as_mut_slice());

        let mut account_b = Account::new(LAMPORTS_PER_SOL, TokenAccount::LEN, &output_token_program);
        let mut account_b_data = TokenAccount::default();
        account_b_data.mint = output_mint;
        account_b_data.owner = keypair.pubkey();
        account_b_data.state = AccountState::Initialized;
        account_b_data.amount = 0;
        account_b_data.pack_into_slice(account_b.data_as_mut_slice());

        litesvm.set_account(source_ata, account_a).unwrap();
        litesvm.set_account(dest_ata, account_b).unwrap();

        let blockhash = litesvm.latest_blockhash();
        let tx =
            Transaction::new_signed_with_payer(&[ix], Some(&keypair.pubkey()), &[keypair], blockhash);

        match litesvm.simulate_transaction(tx) {
            Ok(result) => {
                let account_b = result
                    .post_accounts
                    .into_iter()
                    .find(|(pk, _)| pk == &dest_ata)
                    .map(|(_, acc)| acc)
                    .expect("Destination token account not found in post_accounts");
                let post_b = TokenAccount::unpack_from_slice(account_b.data())
                    .expect("Failed to unpack output token account");
                post_b.amount
            }
            Err(e) => {
                log::error!("Swap simulation failed: {:?}", e.err);
                for line in &e.meta.logs {
                    log::error!("  log: {}", line);
                }
                panic!("Swap simulation failed: {:?}", e.err);
            }
        }
    }

    fn sample_log_uniform_u64(lo: u64, hi: u64) -> u64 {
        assert!(lo >= 1, "log-uniform sampling requires lo >= 1");
        assert!(lo <= hi);

        let lo_f = lo as f64;
        let hi_f = hi as f64;

        let log_lo = lo_f.ln();
        let log_hi = hi_f.ln();

        let r: f64 = rand::rng().random();
        let log_val = log_lo + r * (log_hi - log_lo);

        (log_val.exp() as u64).clamp(lo, hi)
    }

    /// Load AMM from RPC using the Amm trait lifecycle.
    async fn load_amm(rpc_url: &str) -> (ArcherAmm, RpcClient) {
        let market_key_str = env::var("ARCHER_MARKET_KEY").expect("ARCHER_MARKET_KEY must be set");
        let market_key = Pubkey::from_str(&market_key_str).unwrap();

        let rpc = RpcClient::new(rpc_url.to_string());
        let market_account = rpc.get_account(&market_key).await.unwrap();

        let keyed_account = KeyedAccount {
            key: market_key,
            account: market_account,
            params: None,
        };

        let amm_context = AmmContext {
            clock_ref: Default::default(),
        };

        let mut amm = ArcherAmm::from_keyed_account(&keyed_account, &amm_context).unwrap();

        // First update: market + registry
        let rpc2 = RpcClient::new(rpc_url.to_string());
        let accounts = amm.get_accounts_to_update();
        let account_map = fetch_account_map(&rpc2, &accounts).await;
        amm.update(account_map).unwrap();

        // Second update: includes maker books
        let accounts = amm.get_accounts_to_update();
        let account_map = fetch_account_map(&rpc2, &accounts).await;
        amm.update(account_map).unwrap();

        (amm, rpc)
    }

    // -------------------------------------------------------------------------
    // Test 1: check boundary values in simulation
    // -------------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bound_simulation() {
        init_test_logger();

        let rpc_url = env::var("SOLANA_RPC_URL").unwrap();
        let (mut amm, rpc) = load_amm(&rpc_url).await;

        let (mut litesvm, keypair) = setup_litesvm();

        let clock_account = rpc.get_account(&clock::ID).await.unwrap();
        let latest_clock: Clock = bincode::deserialize(&clock_account.data).unwrap();
        litesvm.set_sysvar::<Clock>(&latest_clock);

        // Mirror the simulator clock into the AMM so off-chain staleness/epoch
        // handling matches on-chain execution.
        amm.clock_ref.update(latest_clock.clone());

        snapshot_state_into_svm(&mut amm, &rpc, &mut litesvm, &keypair);

        let mints = amm.get_reserve_mints();
        let base_mint = mints[0];
        let quote_mint = mints[1];

        // Test sell direction FIRST to check for state leakage
        let directions = [
            (base_mint, quote_mint), // sell first
            (quote_mint, base_mint), // buy second
        ];

        for (input_mint, output_mint) in &directions {
            let (lower, upper) = compute_bounds(&amm, input_mint, output_mint);

            for bound in [lower, upper] {
                let quote = amm
                    .quote(&QuoteParams {
                        amount: bound,
                        input_mint: *input_mint,
                        output_mint: *output_mint,
                        swap_mode: SwapMode::ExactIn,
                        fee_mode: FeeMode::Normal,
                    })
                    .unwrap();

                // If the off-chain quote is zero, the market currently has no active
                // liquidity (every maker book stale/empty). On-chain the swap returns
                // NoMatchingLiquidity rather than a 0 output, so there is nothing to
                // cross-check — both agree there is no fill.
                if quote.out_amount == 0 {
                    log::warn!("No liquidity at bound {bound}; skipping sim cross-check");
                    continue;
                }

                let sim =
                    sim_quote_request(&amm, *input_mint, bound, *output_mint, &mut litesvm, &keypair);

                log::debug!(
                    "Boundary = {}\nSimulated = {}\nOff-chain quote = {}\nDelta = {}",
                    bound,
                    sim,
                    quote.out_amount,
                    quote.out_amount.abs_diff(sim)
                );

                assert_eq!(quote.out_amount.abs_diff(sim), 0)
            }
        }
    }

    // -------------------------------------------------------------------------
    // Test 2: Random sampling simulation
    // -------------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn test_random_samples() {
        init_test_logger();

        let rpc_url = env::var("SOLANA_RPC_URL").unwrap();
        let (mut amm, rpc) = load_amm(&rpc_url).await;

        let (mut litesvm, keypair) = setup_litesvm();

        let clock_account = rpc.get_account(&clock::ID).await.unwrap();
        let latest_clock: Clock = bincode::deserialize(&clock_account.data).unwrap();
        litesvm.set_sysvar::<Clock>(&latest_clock);

        amm.clock_ref.update(latest_clock.clone());

        snapshot_state_into_svm(&mut amm, &rpc, &mut litesvm, &keypair);

        let mints = amm.get_reserve_mints();
        let base_mint = mints[0];
        let quote_mint = mints[1];

        let directions = [(quote_mint, base_mint), (base_mint, quote_mint)];

        for (input_mint, output_mint) in &directions {
            let (lb, ub) = compute_bounds(&amm, input_mint, output_mint);

            for _ in 0..50 {
                let amount = sample_log_uniform_u64(lb, ub);

                let quote = amm
                    .quote(&QuoteParams {
                        amount,
                        input_mint: *input_mint,
                        output_mint: *output_mint,
                        swap_mode: SwapMode::ExactIn,
                        fee_mode: FeeMode::Normal,
                    })
                    .unwrap();

                // No active liquidity right now → on-chain returns NoMatchingLiquidity
                // rather than a 0 output; nothing to cross-check.
                if quote.out_amount == 0 {
                    continue;
                }

                let sim = sim_quote_request(
                    &amm,
                    *input_mint,
                    amount,
                    *output_mint,
                    &mut litesvm,
                    &keypair,
                );

                log::debug!(
                    "Random sim: {}\nQuote: {}\nDelta: {}",
                    sim,
                    quote.out_amount,
                    quote.out_amount.abs_diff(sim)
                );

                assert_eq!(quote.out_amount.abs_diff(sim), 0)
            }
        }
    }

    // -------------------------------------------------------------------------
    // Test 3: AMM Monotonicity
    // -------------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn test_monotone() {
        init_test_logger();

        let rpc_url = env::var("SOLANA_RPC_URL").unwrap();
        let (amm, _rpc) = load_amm(&rpc_url).await;

        let mints = amm.get_reserve_mints();
        let base_mint = mints[0];
        let quote_mint = mints[1];

        let directions = [(quote_mint, base_mint), (base_mint, quote_mint)];

        for (input_mint, output_mint) in &directions {
            let (lb, ub) = compute_bounds(&amm, input_mint, output_mint);
            let mut test_amounts = Vec::with_capacity(50);

            for _ in 0..50 {
                test_amounts.push(sample_log_uniform_u64(lb, ub));
            }
            test_amounts.sort();

            let mut prev = 0;
            for amount in test_amounts {
                let result = amm
                    .quote(&QuoteParams {
                        amount,
                        input_mint: *input_mint,
                        output_mint: *output_mint,
                        swap_mode: SwapMode::ExactIn,
                        fee_mode: FeeMode::Normal,
                    })
                    .expect("Quote failed");

                log::debug!("quote: {:?}", result);

                assert!(
                    prev <= result.out_amount,
                    "Swap function is not monotone (prev: {}) > (output: {})",
                    prev,
                    result.out_amount
                );

                prev = result.out_amount;
            }
        }
    }

    // -------------------------------------------------------------------------
    // Test 4: Quoting speed
    // -------------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn test_quoting_speed() {
        init_test_logger();

        let rpc_url = env::var("SOLANA_RPC_URL").unwrap();
        let (amm, _rpc) = load_amm(&rpc_url).await;

        let mints = amm.get_reserve_mints();
        let base_mint = mints[0];
        let quote_mint = mints[1];

        let iterations = 10_000;

        let directions = [(quote_mint, base_mint), (base_mint, quote_mint)];

        for (input_mint, output_mint) in &directions {
            let (lb, ub) = compute_bounds(&amm, input_mint, output_mint);
            let mut test_amounts = Vec::with_capacity(iterations);

            for _ in 0..iterations {
                test_amounts.push(sample_log_uniform_u64(lb, ub));
            }

            let start = Instant::now();
            for amount in test_amounts {
                let result = amm
                    .quote(&QuoteParams {
                        amount,
                        input_mint: *input_mint,
                        output_mint: *output_mint,
                        swap_mode: SwapMode::ExactIn,
                        fee_mode: FeeMode::Normal,
                    })
                    .expect("Quote failed");

                log::debug!("quote: {:?}", result);
            }
            let elapsed = start.elapsed().as_secs_f64();
            let avg_time = elapsed / iterations as f64;

            log::info!("Average quoting speed: {}", avg_time);

            assert!(
                avg_time < 0.0001,
                "Failed quoting speed test for input mint {}",
                input_mint
            );
        }
    }
}
