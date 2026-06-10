/**
 * Coinflip devnet example using @solana/kit and the Codama-generated client.
 *
 * Usage:  npx tsx coinflip.ts [amount_sol] [heads|tails]
 * e.g.    npx tsx coinflip.ts 0.1 heads
 *
 * The typed client in ./generated is produced from the Anchor IDL
 * (idl/coinflip.json) by `node codama.mjs`. It derives all PDAs and account
 * defaults itself, so each instruction below only takes what the caller
 * actually decides.
 *
 * Uses the local Solana CLI wallet (~/.config/solana/id.json) as both the
 * house admin and the player. On first run it initializes the program's
 * config/treasury and seeds the treasury so it can cover the payout.
 */
import {
  createKeyPairSignerFromBytes,
  createSolanaRpc,
  createSolanaRpcSubscriptions,
  createTransactionMessage,
  getSignatureFromTransaction,
  pipe,
  sendAndConfirmTransactionFactory,
  setTransactionMessageFeePayerSigner,
  setTransactionMessageLifetimeUsingBlockhash,
  signTransactionMessageWithSigners,
  appendTransactionMessageInstructions,
  assertIsTransactionWithBlockhashLifetime,
  type Address,
  type KeyPairSigner,
  type Signature,
} from "@solana/kit";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import {
  Choice,
  fetchBet,
  fetchConfig,
  fetchMaybeConfig,
  findBetPda,
  findConfigPda,
  findTreasuryPda,
  getFundTreasuryInstructionAsync,
  getInitializeInstructionAsync,
  getPlaceBetInstructionAsync,
  getSettleBetInstructionAsync,
} from "./generated/src/generated/index.js";

const RPC_URL = process.env.RPC_URL ?? "https://api.devnet.solana.com";
const WS_URL = process.env.WS_URL ?? RPC_URL.replace("https", "wss");

const LAMPORTS_PER_SOL = 1_000_000_000n;
const TREASURY_RENT_MIN = 890_880n; // rent-exempt minimum for a 0-byte account

const rpc = createSolanaRpc(RPC_URL);
const rpcSubscriptions = createSolanaRpcSubscriptions(WS_URL);
const sendAndConfirm = sendAndConfirmTransactionFactory({ rpc, rpcSubscriptions });

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

type Ix = Parameters<typeof appendTransactionMessageInstructions>[0][number];

async function sendTx(payer: KeyPairSigner, ixs: Ix[]): Promise<Signature> {
  const { value: latestBlockhash } = await rpc.getLatestBlockhash().send();
  const message = pipe(
    createTransactionMessage({ version: 0 }),
    (m) => setTransactionMessageFeePayerSigner(payer, m),
    (m) => setTransactionMessageLifetimeUsingBlockhash(latestBlockhash, m),
    (m) => appendTransactionMessageInstructions(ixs, m),
  );
  const signed = await signTransactionMessageWithSigners(message);
  assertIsTransactionWithBlockhashLifetime(signed);
  await sendAndConfirm(signed, { commitment: "confirmed" });
  return getSignatureFromTransaction(signed);
}

async function getBalance(addr: Address): Promise<bigint> {
  const { value } = await rpc.getBalance(addr).send();
  return BigInt(value);
}

async function main() {
  const amountSol = Number(process.argv[2] ?? "0.1");
  const choiceArg = (process.argv[3] ?? "heads").toLowerCase();
  if (!["heads", "tails"].includes(choiceArg) || !(amountSol >= 0.1 && amountSol <= 10)) {
    console.error("usage: tsx coinflip.ts <amount 0.1..10> <heads|tails>");
    process.exit(1);
  }
  const choice = choiceArg === "heads" ? Choice.Heads : Choice.Tails;
  const amount = BigInt(Math.round(amountSol * 1e9));
  const payout = amount * 2n;

  const wallet = await createKeyPairSignerFromBytes(
    new Uint8Array(JSON.parse(readFileSync(`${homedir()}/.config/solana/id.json`, "utf8"))),
  );
  console.log(`wallet   ${wallet.address}`);
  console.log(`balance  ${Number(await getBalance(wallet.address)) / 1e9} SOL`);

  const [config] = await findConfigPda();
  const [treasury] = await findTreasuryPda();

  // --- one-time setup: initialize config + treasury ------------------------
  if (!(await fetchMaybeConfig(rpc, config)).exists) {
    console.log("initializing config + treasury…");
    await sendTx(wallet, [await getInitializeInstructionAsync({ admin: wallet })]);
  }

  // --- make sure the treasury can cover this bet's payout ------------------
  const { data: configData } = await fetchConfig(rpc, config);
  const free = (await getBalance(treasury)) - TREASURY_RENT_MIN - configData.lockedLamports;
  if (free < amount) {
    const topUp = payout - free + LAMPORTS_PER_SOL / 10n;
    console.log(`funding treasury with ${Number(topUp) / 1e9} SOL of house liquidity…`);
    await sendTx(wallet, [
      await getFundTreasuryInstructionAsync({ funder: wallet, amount: topUp }),
    ]);
  }

  // --- place the bet --------------------------------------------------------
  const nonce = BigInt(Date.now());
  const [bet] = await findBetPda({ player: wallet.address, nonce });

  console.log(`\nbetting ${amountSol} SOL on ${choiceArg} (bet account ${bet})`);
  const placeSig = await sendTx(wallet, [
    await getPlaceBetInstructionAsync({ player: wallet, nonce, amount, choice }),
  ]);
  console.log(`placed   https://explorer.solana.com/tx/${placeSig}?cluster=devnet`);

  const { data: betData } = await fetchBet(rpc, bet);
  const targetSlot = betData.targetSlot;

  // --- wait for the deciding slot, then settle ------------------------------
  process.stdout.write(`waiting for slot ${targetSlot} to land`);
  while ((await rpc.getSlot({ commitment: "confirmed" }).send()) <= targetSlot) {
    process.stdout.write(".");
    await sleep(400);
  }
  console.log();

  const balanceBefore = await getBalance(wallet.address);
  const settleIx = await getSettleBetInstructionAsync({ bet, player: wallet.address });

  // The settle window is target_slot + 512 slots (~3.5 min); retry a couple
  // of times in case the RPC's view of SlotHashes lags right at the edge.
  let settleSig: Signature | undefined;
  for (let attempt = 1; !settleSig; attempt++) {
    try {
      settleSig = await sendTx(wallet, [settleIx]);
    } catch (e) {
      if (attempt >= 5) throw e;
      await sleep(1000);
    }
  }
  console.log(`settled  https://explorer.solana.com/tx/${settleSig}?cluster=devnet`);

  // --- decode the BetSettled event from the logs ----------------------------
  // (Codama does not yet render Anchor event decoders, so this stays manual.)
  const tx = await rpc
    .getTransaction(settleSig, {
      commitment: "confirmed",
      encoding: "json",
      maxSupportedTransactionVersion: 0,
    })
    .send();
  const eventDisc = createHash("sha256").update("event:BetSettled").digest().subarray(0, 8);
  for (const log of tx?.meta?.logMessages ?? []) {
    if (!log.startsWith("Program data: ")) continue;
    const data = Buffer.from(log.slice("Program data: ".length), "base64");
    if (!data.subarray(0, 8).equals(eventDisc)) continue;
    // BetSettled: player 32 | nonce 8 | amount 8 | choice 1 | result 1 | win 1 | payout 8
    const dv = new DataView(data.buffer, data.byteOffset);
    const result = data[57] === Choice.Heads ? "heads" : "tails";
    const win = data[58] === 1;
    const paid = dv.getBigUint64(59, true);
    console.log(`\nthe coin landed on ${result.toUpperCase()} — you ${win ? "WIN" : "lose"}!`);
    if (win) console.log(`payout: ${Number(paid) / 1e9} SOL (net +${amountSol} SOL)`);
  }
  const balanceAfter = await getBalance(wallet.address);
  console.log(`balance change this settlement: ${Number(balanceAfter - balanceBefore) / 1e9} SOL`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
