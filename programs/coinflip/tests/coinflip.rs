//! Integration tests: run the compiled SBF program in LiteSVM and drive the
//! clock / SlotHashes sysvar to make outcomes deterministic.
//!
//! Build the program first: `cargo build-sbf`

use litesvm::LiteSVM;
use solana_program::slot_hashes::SlotHashes;
use solana_sdk::{
    clock::Clock,
    hash::hashv,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_program,
    sysvar::slot_hashes,
    transaction::Transaction,
};
use std::str::FromStr;

const MIN_BET: u64 = 100_000_000;
const MAX_BET: u64 = 10_000_000_000;
const WIN_PERCENT: u64 = 49;
const COMMIT_DELAY_SLOTS: u64 = 2;
const SETTLE_WINDOW_SLOTS: u64 = 512;
const SOL: u64 = 1_000_000_000;
const TREASURY_RENT_MIN: u64 = 890_880; // rent-exempt minimum for 0 bytes

fn program_id() -> Pubkey {
    Pubkey::from_str("7ffE4JF4ZNCmnxQZWFxFT3ny9VDsf3LDJ1vbUNjLspX3").unwrap()
}

fn disc(name: &str) -> [u8; 8] {
    let h = solana_sdk::hash::hash(format!("global:{name}").as_bytes());
    h.to_bytes()[..8].try_into().unwrap()
}

struct Env {
    svm: LiteSVM,
    admin: Keypair,
    config: Pubkey,
    treasury: Pubkey,
}

impl Env {
    fn new() -> Self {
        let mut svm = LiteSVM::new();
        let pid = program_id();
        let so_path = format!(
            "{}/../../target/deploy/coinflip.so",
            env!("CARGO_MANIFEST_DIR")
        );
        svm.add_program_from_file(pid, &so_path)
            .expect("run `cargo build-sbf` before `cargo test`");

        let admin = Keypair::new();
        svm.airdrop(&admin.pubkey(), 1_000 * SOL).unwrap();

        let (config, _) = Pubkey::find_program_address(&[b"config"], &pid);
        let (treasury, _) = Pubkey::find_program_address(&[b"treasury"], &pid);

        let mut env = Env {
            svm,
            admin,
            config,
            treasury,
        };
        let ix = Instruction {
            program_id: pid,
            accounts: vec![
                AccountMeta::new(env.admin.pubkey(), true),
                AccountMeta::new(env.config, false),
                AccountMeta::new(env.treasury, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data: disc("initialize").to_vec(),
        };
        env.send(&[ix], &env.admin.insecure_clone()).unwrap();
        env
    }

    fn send(
        &mut self,
        ixs: &[Instruction],
        payer: &Keypair,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let tx = Transaction::new_signed_with_payer(
            ixs,
            Some(&payer.pubkey()),
            &[payer],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e.err).into())
    }

    fn fund_treasury(&mut self, amount: u64) {
        let ix = Instruction {
            program_id: program_id(),
            accounts: vec![
                AccountMeta::new(self.admin.pubkey(), true),
                AccountMeta::new(self.treasury, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data: [disc("fund_treasury").as_slice(), &amount.to_le_bytes()].concat(),
        };
        self.send(&[ix], &self.admin.insecure_clone()).unwrap();
    }

    fn bet_pda(&self, player: &Pubkey, nonce: u64) -> Pubkey {
        Pubkey::find_program_address(
            &[b"bet", player.as_ref(), &nonce.to_le_bytes()],
            &program_id(),
        )
        .0
    }

    /// Returns the bet PDA and its target slot.
    fn place_bet(
        &mut self,
        player: &Keypair,
        nonce: u64,
        amount: u64,
        choice: u8,
    ) -> Result<(Pubkey, u64), Box<dyn std::error::Error>> {
        let bet = self.bet_pda(&player.pubkey(), nonce);
        let target_slot = self.svm.get_sysvar::<Clock>().slot + COMMIT_DELAY_SLOTS;
        let mut data = disc("place_bet").to_vec();
        data.extend_from_slice(&nonce.to_le_bytes());
        data.extend_from_slice(&amount.to_le_bytes());
        data.push(choice);
        let ix = Instruction {
            program_id: program_id(),
            accounts: vec![
                AccountMeta::new(player.pubkey(), true),
                AccountMeta::new(self.config, false),
                AccountMeta::new(self.treasury, false),
                AccountMeta::new(bet, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data,
        };
        self.send(&[ix], player)?;
        Ok((bet, target_slot))
    }

    fn settle_ix(&self, bet: Pubkey, player: Pubkey) -> Instruction {
        Instruction {
            program_id: program_id(),
            accounts: vec![
                AccountMeta::new(self.config, false),
                AccountMeta::new(self.treasury, false),
                AccountMeta::new(bet, false),
                AccountMeta::new(player, false),
                AccountMeta::new_readonly(slot_hashes::ID, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data: disc("settle_bet").to_vec(),
        }
    }

    fn expire_ix(&self, bet: Pubkey, player: Pubkey) -> Instruction {
        Instruction {
            program_id: program_id(),
            accounts: vec![
                AccountMeta::new(self.config, false),
                AccountMeta::new(bet, false),
                AccountMeta::new(player, false),
            ],
            data: disc("expire_bet").to_vec(),
        }
    }

    /// Warp past the target slot and install a slot hash that makes the bet
    /// win or lose as requested.
    fn rig_outcome(&mut self, bet: Pubkey, target_slot: u64, want_win: bool) {
        let hash = find_rigged_hash(bet, target_slot, want_win);
        self.svm.warp_to_slot(target_slot + 1);
        self.svm
            .set_sysvar::<SlotHashes>(&SlotHashes::new(&[(target_slot, hash.into())]));
    }

    fn balance(&self, key: &Pubkey) -> u64 {
        self.svm.get_balance(key).unwrap_or(0)
    }

    fn locked_lamports(&self) -> u64 {
        let data = self.svm.get_account(&self.config).unwrap().data;
        // Config layout: 8-byte discriminator, 32-byte admin, u64 locked.
        u64::from_le_bytes(data[40..48].try_into().unwrap())
    }

    fn new_player(&mut self, lamports: u64) -> Keypair {
        let kp = Keypair::new();
        self.svm.airdrop(&kp.pubkey(), lamports).unwrap();
        kp
    }
}

/// Mirror of the program's outcome derivation.
fn roll_for(slot_hash: [u8; 32], bet: Pubkey, target_slot: u64) -> u64 {
    let d = hashv(&[&slot_hash, bet.as_ref(), &target_slot.to_le_bytes()]);
    u64::from_le_bytes(d.to_bytes()[..8].try_into().unwrap()) % 100
}

fn find_rigged_hash(bet: Pubkey, target_slot: u64, want_win: bool) -> [u8; 32] {
    for i in 0u64.. {
        let mut h = [0u8; 32];
        h[..8].copy_from_slice(&i.to_le_bytes());
        if (roll_for(h, bet, target_slot) < WIN_PERCENT) == want_win {
            return h;
        }
    }
    unreachable!()
}

#[test]
fn win_pays_double_and_releases_lock() {
    let mut env = Env::new();
    env.fund_treasury(50 * SOL);
    let player = env.new_player(10 * SOL);

    let amount = SOL;
    let (bet, target_slot) = env.place_bet(&player, 0, amount, 0).unwrap();
    assert_eq!(env.locked_lamports(), 2 * amount);

    let treasury_after_bet = env.balance(&env.treasury);
    let player_after_bet = env.balance(&player.pubkey());

    env.rig_outcome(bet, target_slot, true);

    // Settlement is permissionless: a random cranker settles.
    let cranker = env.new_player(SOL);
    let ix = env.settle_ix(bet, player.pubkey());
    env.send(&[ix], &cranker).unwrap();

    // Player receives the 2x payout plus the bet account rent.
    let bet_rent = player_after_bet
        + amount // what they should net
        + amount; // their stake back
    assert!(env.balance(&player.pubkey()) > bet_rent); // payout + rent refund
    assert_eq!(env.balance(&env.treasury), treasury_after_bet - 2 * amount);
    assert_eq!(env.locked_lamports(), 0);
    assert!(env.svm.get_account(&bet).is_none() || env.balance(&bet) == 0);
}

#[test]
fn loss_keeps_bet_in_treasury() {
    let mut env = Env::new();
    env.fund_treasury(50 * SOL);
    let player = env.new_player(10 * SOL);

    let amount = SOL;
    let (bet, target_slot) = env.place_bet(&player, 0, amount, 1).unwrap();
    let treasury_after_bet = env.balance(&env.treasury);
    let player_after_bet = env.balance(&player.pubkey());

    env.rig_outcome(bet, target_slot, false);
    let ix = env.settle_ix(bet, player.pubkey());
    let cranker = env.new_player(SOL);
    env.send(&[ix], &cranker).unwrap();

    // Treasury keeps the bet; player only got the bet account rent back.
    assert_eq!(env.balance(&env.treasury), treasury_after_bet);
    assert!(env.balance(&player.pubkey()) < player_after_bet + amount / 100);
    assert_eq!(env.locked_lamports(), 0);
}

#[test]
fn bet_size_limits_enforced() {
    let mut env = Env::new();
    env.fund_treasury(50 * SOL);
    let player = env.new_player(20 * SOL);

    assert!(env.place_bet(&player, 0, MIN_BET - 1, 0).is_err());
    assert!(env.place_bet(&player, 1, MAX_BET + 1, 0).is_err());
    assert!(env.place_bet(&player, 2, MIN_BET, 0).is_ok());
    assert!(env.place_bet(&player, 3, MAX_BET, 0).is_ok());
}

#[test]
fn insolvent_treasury_rejects_bets() {
    let mut env = Env::new();
    // Treasury can only cover 1 SOL of exposure.
    env.fund_treasury(SOL);
    let player = env.new_player(20 * SOL);

    // 2 SOL bet would need 2 SOL of house exposure: rejected.
    assert!(env.place_bet(&player, 0, 2 * SOL, 0).is_err());
    // 1 SOL bet exactly matches available liquidity: accepted.
    assert!(env.place_bet(&player, 1, SOL, 0).is_ok());
    // Liquidity is now fully locked; even the minimum bet is rejected
    // until the pending bet settles.
    assert!(env.place_bet(&player, 2, MIN_BET, 0).is_err());
}

#[test]
fn cannot_settle_before_target_slot() {
    let mut env = Env::new();
    env.fund_treasury(50 * SOL);
    let player = env.new_player(10 * SOL);

    let (bet, target_slot) = env.place_bet(&player, 0, SOL, 0).unwrap();
    // SlotHashes even *contains* the (future) slot — settling must still
    // fail because the clock has not passed the target slot.
    env.svm
        .set_sysvar::<SlotHashes>(&SlotHashes::new(&[(target_slot, [7u8; 32].into())]));
    let ix = env.settle_ix(bet, player.pubkey());
    let cranker = env.new_player(SOL);
    assert!(env.send(&[ix], &cranker).is_err());
    assert_eq!(env.locked_lamports(), 2 * SOL); // still pending
}

#[test]
fn cannot_settle_twice() {
    let mut env = Env::new();
    env.fund_treasury(50 * SOL);
    let player = env.new_player(10 * SOL);

    let (bet, target_slot) = env.place_bet(&player, 0, SOL, 0).unwrap();
    env.rig_outcome(bet, target_slot, true);
    let cranker = env.new_player(SOL);
    env.send(&[env.settle_ix(bet, player.pubkey())], &cranker)
        .unwrap();
    // The bet account was closed; settling again must fail.
    assert!(env
        .send(&[env.settle_ix(bet, player.pubkey())], &cranker)
        .is_err());
    assert_eq!(env.locked_lamports(), 0);
}

#[test]
fn expired_bet_forfeits_to_treasury() {
    let mut env = Env::new();
    env.fund_treasury(50 * SOL);
    let player = env.new_player(10 * SOL);

    let amount = SOL;
    let (bet, target_slot) = env.place_bet(&player, 0, amount, 0).unwrap();
    let treasury_after_bet = env.balance(&env.treasury);

    // Too early to expire while the settle window is open.
    env.svm.warp_to_slot(target_slot + 1);
    let cranker = env.new_player(SOL);
    assert!(env
        .send(&[env.expire_ix(bet, player.pubkey())], &cranker)
        .is_err());

    // Past the window: settling fails (window passed), expiring succeeds.
    env.svm.warp_to_slot(target_slot + SETTLE_WINDOW_SLOTS + 1);
    // Rotate the blockhash so the retried expire tx isn't a duplicate
    // signature of the earlier (intentionally failing) attempt.
    env.svm.expire_blockhash();
    env.svm
        .set_sysvar::<SlotHashes>(&SlotHashes::new(&[(target_slot, [7u8; 32].into())]));
    assert!(env
        .send(&[env.settle_ix(bet, player.pubkey())], &cranker)
        .is_err());
    env.send(&[env.expire_ix(bet, player.pubkey())], &cranker)
        .unwrap();

    // The bet stays in the treasury, the lock is released, the bet is gone.
    assert_eq!(env.balance(&env.treasury), treasury_after_bet);
    assert_eq!(env.locked_lamports(), 0);
    assert!(env.svm.get_account(&bet).is_none() || env.balance(&bet) == 0);
}

#[test]
fn withdraw_respects_locked_funds_and_admin_only() {
    let mut env = Env::new();
    env.fund_treasury(10 * SOL);
    let player = env.new_player(10 * SOL);
    env.place_bet(&player, 0, SOL, 0).unwrap();

    let withdraw = |env: &Env, amount: u64, dest: Pubkey| Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new_readonly(env.admin.pubkey(), true),
            AccountMeta::new_readonly(env.config, false),
            AccountMeta::new(env.treasury, false),
            AccountMeta::new(dest, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: [disc("withdraw_house").as_slice(), &amount.to_le_bytes()].concat(),
    };

    // Treasury holds 10 (house) + 1 (bet) SOL + rent; 2 SOL is locked for
    // the pending payout, so at most 9 SOL is withdrawable.
    let admin = env.admin.insecure_clone();
    let dest = Keypair::new().pubkey();
    assert!(env.send(&[withdraw(&env, 9 * SOL + 1, dest)], &admin).is_err());
    env.send(&[withdraw(&env, 9 * SOL, dest)], &admin).unwrap();
    assert_eq!(env.balance(&dest), 9 * SOL);
    assert_eq!(
        env.balance(&env.treasury),
        2 * SOL + TREASURY_RENT_MIN // exactly the locked payout + rent floor
    );

    // A non-admin signer must be rejected.
    let mallory = env.new_player(SOL);
    let mut ix = withdraw(&env, 1, mallory.pubkey());
    ix.accounts[0] = AccountMeta::new_readonly(mallory.pubkey(), true);
    assert!(env.send(&[ix], &mallory).is_err());
}

#[test]
fn statistical_house_edge() {
    // Sanity-check the outcome derivation: over many random slot hashes the
    // win rate must approximate 49%.
    let bet = Pubkey::new_unique();
    let mut wins = 0u32;
    let n = 100_000u32;
    for i in 0..n {
        let mut h = [0u8; 32];
        h[..4].copy_from_slice(&i.to_le_bytes());
        let h = hashv(&[&h]).to_bytes(); // diffuse the counter
        if roll_for(h, bet, 42) < WIN_PERCENT {
            wins += 1;
        }
    }
    let rate = wins as f64 / n as f64;
    assert!((rate - 0.49).abs() < 0.005, "win rate was {rate}");
}
