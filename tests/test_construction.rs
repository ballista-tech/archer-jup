#[cfg(test)]
mod test_construction {
    use std::collections::HashMap;
    use std::env;
    use std::str::FromStr;

    use solana_client::nonblocking::rpc_client::RpcClient;
    use solana_pubkey::Pubkey;
    use solana_sdk::account::Account;

    use jupiter_amm_interface::{Amm, AmmContext, FeeMode, KeyedAccount, QuoteParams, SwapMode};

    use archer_jup::ArcherAmm;

    type AccountMap = HashMap<Pubkey, Account>;

    fn init_test_logger() {
        let _ = dotenvy::dotenv();
        let _ = env_logger::builder().is_test(true).try_init();
    }

    async fn fetch_account_map(rpc: &RpcClient, keys: &[Pubkey]) -> AccountMap {
        let mut map = AccountMap::default();
        for key in keys {
            if let Ok(account) = rpc.get_account(key).await {
                map.insert(*key, account);
            }
        }
        map
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

    /// Set ARCHER_MARKET_KEY and SOLANA_RPC_URL environment variables to run.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_construction() {
        init_test_logger();

        let market_key_str = env::var("ARCHER_MARKET_KEY")
            .expect("ARCHER_MARKET_KEY must be set for integration tests");
        let market_key = Pubkey::from_str(&market_key_str).expect("Invalid market pubkey");

        let rpc_url =
            env::var("SOLANA_RPC_URL").expect("SOLANA_RPC_URL must be set for integration tests");
        let rpc = RpcClient::new(rpc_url.clone());

        let market_account = rpc
            .get_account(&market_key)
            .await
            .expect("Failed to fetch market account");

        let keyed_account = KeyedAccount {
            key: market_key,
            account: market_account,
            params: None,
        };

        let amm_context = AmmContext {
            clock_ref: Default::default(),
        };

        let mut amm = ArcherAmm::from_keyed_account(&keyed_account, &amm_context)
            .expect("Failed to construct AMM from account");

        // First update: market + registry
        let accounts = amm.get_accounts_to_update();
        let account_map = fetch_account_map(&rpc, &accounts).await;
        amm.update(account_map).expect("First update failed");

        // Second update: includes maker books
        let accounts = amm.get_accounts_to_update();
        let account_map = fetch_account_map(&rpc, &accounts).await;
        amm.update(account_map).expect("Second update failed");

        assert!(amm.is_active(), "AMM should be active after state update");

        let mints = amm.get_reserve_mints();
        assert_eq!(mints.len(), 2, "Archer markets always have 2 tokens");
        let base_mint = mints[0];
        let quote_mint = mints[1];

        let token_pairs = [
            (quote_mint, base_mint), // buy direction: quote -> base
            (base_mint, quote_mint), // sell direction: base -> quote
        ];

        for (input_mint, output_mint) in &token_pairs {
            log::info!("Checking bounds for input mint: {}", input_mint);

            let (lower_bound, upper_bound) = compute_bounds(&amm, input_mint, output_mint);

            assert!(
                lower_bound <= upper_bound,
                "Lower bound must be <= upper bound"
            );

            let lb_result = amm
                .quote(&QuoteParams {
                    amount: lower_bound,
                    input_mint: *input_mint,
                    output_mint: *output_mint,
                    swap_mode: SwapMode::ExactIn,
                    fee_mode: FeeMode::Normal,
                })
                .expect("Lower-bound quote failed");

            assert!(lb_result.out_amount > 0, "Lower bound produced zero output");

            let ub_result = amm
                .quote(&QuoteParams {
                    amount: upper_bound,
                    input_mint: *input_mint,
                    output_mint: *output_mint,
                    swap_mode: SwapMode::ExactIn,
                    fee_mode: FeeMode::Normal,
                })
                .expect("Upper-bound quote failed");

            assert!(ub_result.out_amount > 0, "Upper bound produced zero output");
        }
    }
}
