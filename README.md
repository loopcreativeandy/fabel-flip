# Solana Coinflip

An Anchor program where players bet **0.1 – 10 SOL** on heads or tails.
The flip lands on the player's pick with **49%** probability and on the
opposite side with **51%**, so the house has a 2% expected edge. A win pays
out **2× the bet** (the player nets +bet); a loss forfeits the bet to the
treasury.

**Deployed on devnet:** [`7ffE4JF4ZNCmnxQZWFxFT3ny9VDsf3LDJ1vbUNjLspX3`](https://explorer.solana.com/address/7ffE4JF4ZNCmnxQZWFxFT3ny9VDsf3LDJ1vbUNjLspX3?cluster=devnet)

```
programs/coinflip/src/lib.rs        the program
programs/coinflip/tests/coinflip.rs LiteSVM integration tests
examples/coinflip.ts                @solana/kit example client (devnet)
```

## How a bet works

```
tx 1: place_bet(nonce, amount, choice)
      ├─ validates 0.1 SOL <= amount <= 10 SOL
      ├─ checks the treasury can pay the 2x payout, locks it
      ├─ moves the bet into the treasury
      └─ commits to target_slot = current_slot + 2   (does not exist yet!)

  ... target_slot is produced by the cluster ...

tx 2: settle_bet   (anyone can call — player, house crank, anybody)
      ├─ reads target_slot's hash from the SlotHashes sysvar
      ├─ roll = sha256(slot_hash, bet_pubkey, target_slot) % 100
      ├─ roll < 49  → player wins: treasury pays 2x bet to the player
      ├─ roll >= 49 → house wins: bet stays in the treasury
      └─ unlocks the reserved payout, closes the bet (rent → player)
```

If a bet is not settled within **512 slots** (~3.5 min — SlotHashes only
retains the latest 512 hashes), `expire_bet` forfeits it to the house and
releases the lock. In production the house runs a tiny crank that settles
every bet as soon as its target slot lands, so expiry never hits honest
players.

## Instructions

| instruction      | who        | purpose |
|------------------|------------|---------|
| `initialize`     | deployer   | create config + rent-exempt treasury PDA |
| `fund_treasury`  | anyone     | add house liquidity |
| `place_bet`      | player     | commit a bet against a future slot |
| `settle_bet`     | anyone     | resolve a bet after its target slot |
| `expire_bet`     | anyone     | forfeit a bet whose settle window passed |
| `withdraw_house` | admin only | withdraw *unlocked* profit |

## Why the treasury can't be drained

* **No same-transaction or simulated randomness.** The outcome depends on the
  hash of a slot that does not exist when the bet is placed. A bot cannot
  simulate the flip and only submit winning bets — there is nothing to
  simulate yet.
* **No selective settlement.** Once the target slot lands the outcome is
  fixed and *anyone* may settle (payout always goes to the recorded player).
  Letting a losing bet rot doesn't help: after the 512-slot window it
  forfeits to the house via `expire_bet`. Expiry deliberately favors the
  house — a refund here would let players settle only their winners.
* **Solvency is enforced, not assumed.** Every accepted bet reserves its full
  2× payout in `Config.locked_lamports`; `place_bet` rejects any bet the
  treasury couldn't pay, and `withdraw_house` can never touch reserved
  lamports or the treasury's rent floor. The program can never owe more than
  it holds.
* **No double settlement.** Settling or expiring closes the bet account
  (Anchor `close`), so it cannot be replayed; PDAs + `has_one` constraints
  pin every account to the right bet, player, and sysvar address.
* **Checked arithmetic everywhere**, plus `overflow-checks = true` in the
  release profile.
* **Bet caps (0.1–10 SOL)** bound the worst-case loss of any single flip.

### Known limitation (read before mainnet)

Slot-hash randomness is unpredictable to ordinary users and bots, but the
**leader of the target slot** has some influence over its hash. A staked
validator could time bets so it is the leader two slots later and grind block
contents to bias outcomes. The 10 SOL cap bounds the damage, but for serious
treasury sizes swap the randomness source for an oracle VRF (e.g. Switchboard
On-Demand Randomness): keep `place_bet`/`settle_bet` exactly as they are and
replace the slot-hash lookup with the VRF reveal. Everything else (locking,
expiry, permissionless settlement) stays valid.

## Build & test

```sh
cargo build-sbf          # builds target/deploy/coinflip.so
cargo test               # runs the LiteSVM integration tests
```

The tests cover: win/loss payouts and lock release, bet size limits,
insolvency rejection, settling too early, double settlement, expiry
forfeiture, withdraw limits + admin gating, and a 100k-sample statistical
check that the win rate is 49%.

## Try it on devnet

[examples/coinflip.ts](examples/coinflip.ts) is a self-contained client
built on [@solana/kit](https://github.com/anza-xyz/kit). It uses your Solana
CLI wallet (`~/.config/solana/id.json`), initializes the program's
config/treasury on first run, tops up house liquidity when needed, places a
bet, waits for the deciding slot, settles, and decodes the `BetSettled`
event to print the result:

```sh
cd examples
npm install
npx tsx coinflip.ts 0.1 heads
```

```
betting 0.1 SOL on heads (bet account EUEUhMja…)
placed   https://explorer.solana.com/tx/…?cluster=devnet
waiting for slot 468452521 to land...
settled  https://explorer.solana.com/tx/…?cluster=devnet

the coin landed on HEADS — you WIN!
payout: 0.2 SOL (net +0.1 SOL)
```

To redeploy under your own program ID: regenerate
`target/deploy/coinflip-keypair.json`, put its pubkey in `declare_id!`
(and in the test + example constants), `cargo build-sbf`, then
`solana program deploy target/deploy/coinflip.so --program-id target/deploy/coinflip-keypair.json`.
