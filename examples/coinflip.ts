/**
 * Coinflip devnet example using @solana/kit.
 *
 * Usage:  npx tsx coinflip.ts [amount_sol] [heads|tails]
 * e.g.    npx tsx coinflip.ts 0.1 heads
 *
 * Uses the local Solana CLI wallet (~/.config/solana/id.json) as both the
 * house admin and the player. On first run it initializes the program's
 * config/treasury and seeds the treasury so it can cover the payout.
 */
import {
  address,
  AccountRole,
  appendTransactionMessageInstructions,
  createKeyPairSignerFromBytes,
  createSolanaRpc,
  createSolanaRpcSubscriptions,
  createTransactionMessage,
  getAddressEncoder,
  getProgramDerivedAddress,
  getSignatureFromTransaction,
  pipe,
  sendAndConfirmTransactionFactory,
  setTransactionMessageFeePayerSigner,
  setTransactionMessageLifetimeUsingBlockhash,
  signTransactionMessageWithSigners,
  type AccountMeta,
  type AccountSignerMeta,
  type Instruction,
  type KeyPairSigner,
  type Signature,
} from "@solana/kit";

type Ix = Instruction<string, readonly (AccountMeta | AccountSignerMeta)[]>;
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { homedir } from "node:os";

const PROGRAM_ID = address("7ffE4JF4ZNCmnxQZWFxFT3ny9VDsf3LDJ1vbUNjLspX3");
const SYSTEM_PROGRAM = address("11111111111111111111111111111111");
const SLOT_HASHES_SYSVAR = address("SysvarS1otHashes111111111111111111111111111");

const RPC_URL = process.env.RPC_URL ?? "https://api.devnet.solana.com";
const WS_URL = process.env.WS_URL ?? RPC_URL.replace("https", "wss");

const LAMPORTS_PER_SOL = 1_000_000_000n;
const TREASURY_RENT_MIN = 890_880n; // rent-exempt minimum for a 0-byte account
const SETTLE_WINDOW_SLOTS = 512n;

const rpc = createSolanaRpc(RPC_URL);
const rpcSubscriptions = createSolanaRpcSubscriptions(WS_URL);
const sendAndConfirm = sendAndConfirmTransactionFactory({ rpc, rpcSubscriptions });
const addressEncoder = getAddressEncoder();

/** Anchor instruction discriminator: sha256("global:<name>")[..8]. */
const disc = (name: string): Uint8Array =>
  createHash("sha256").update(`global:${name}`).digest().subarray(0, 8);

const u64Le = (n: bigint): Uint8Array => {
  const buf = new Uint8Array(8);
  new DataView(buf.buffer).setBigUint64(0, n, true);
  return buf;
};

const concat = (...parts: Uint8Array[]): Uint8Array => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.length;
  }
  return out;
};

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function sendTx(payer: KeyPairSigner, ixs: Ix[]): Promise<Signature> {
  const { value: latestBlockhash } = await rpc.getLatestBlockhash().send();
  const message = pipe(
    createTransactionMessage({ version: 0 }),
    (m) => setTransactionMessageFeePayerSigner(payer, m),
    (m) => setTransactionMessageLifetimeUsingBlockhash(latestBlockhash, m),
    (m) => appendTransactionMessageInstructions(ixs, m),
  );
  const signed = await signTransactionMessageWithSigners(message);
  await sendAndConfirm(signed, { commitment: "confirmed" });
  return getSignatureFromTransaction(signed);
}

async function fetchAccountData(addr: ReturnType<typeof address>): Promise<Uint8Array | null> {
  const { value } = await rpc.getAccountInfo(addr, { encoding: "base64" }).send();
  return value ? Buffer.from(value.data[0], "base64") : null;
}

async function getBalance(addr: ReturnType<typeof address>): Promise<bigint> {
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
  const choice = choiceArg === "heads" ? 0 : 1;
  const amount = BigInt(Math.round(amountSol * 1e9));
  const payout = amount * 2n;

  const wallet = await createKeyPairSignerFromBytes(
    new Uint8Array(JSON.parse(readFileSync(`${homedir()}/.config/solana/id.json`, "utf8"))),
  );
  console.log(`wallet   ${wallet.address}`);
  console.log(`balance  ${Number(await getBalance(wallet.address)) / 1e9} SOL`);

  const [config] = await getProgramDerivedAddress({ programAddress: PROGRAM_ID, seeds: ["config"] });
  const [treasury] = await getProgramDerivedAddress({ programAddress: PROGRAM_ID, seeds: ["treasury"] });

  // --- one-time setup: initialize config + treasury ------------------------
  if (!(await fetchAccountData(config))) {
    console.log("initializing config + treasury…");
    await sendTx(wallet, [
      {
        programAddress: PROGRAM_ID,
        accounts: [
          { address: wallet.address, role: AccountRole.WRITABLE_SIGNER, signer: wallet },
          { address: config, role: AccountRole.WRITABLE },
          { address: treasury, role: AccountRole.WRITABLE },
          { address: SYSTEM_PROGRAM, role: AccountRole.READONLY },
        ],
        data: disc("initialize"),
      },
    ]);
  }

  // --- make sure the treasury can cover this bet's payout ------------------
  // Config layout: 8 discriminator | 32 admin | 8 locked_lamports | bumps.
  const configData = (await fetchAccountData(config))!;
  const locked = new DataView(configData.buffer, configData.byteOffset).getBigUint64(40, true);
  const free = (await getBalance(treasury)) - TREASURY_RENT_MIN - locked;
  if (free < amount) {
    const topUp = payout - free + LAMPORTS_PER_SOL / 10n;
    console.log(`funding treasury with ${Number(topUp) / 1e9} SOL of house liquidity…`);
    await sendTx(wallet, [
      {
        programAddress: PROGRAM_ID,
        accounts: [
          { address: wallet.address, role: AccountRole.WRITABLE_SIGNER, signer: wallet },
          { address: treasury, role: AccountRole.WRITABLE },
          { address: SYSTEM_PROGRAM, role: AccountRole.READONLY },
        ],
        data: concat(disc("fund_treasury"), u64Le(topUp)),
      },
    ]);
  }

  // --- place the bet --------------------------------------------------------
  const nonce = BigInt(Date.now());
  const [bet] = await getProgramDerivedAddress({
    programAddress: PROGRAM_ID,
    seeds: ["bet", addressEncoder.encode(wallet.address), u64Le(nonce)],
  });

  console.log(`\nbetting ${amountSol} SOL on ${choiceArg} (bet account ${bet})`);
  const placeSig = await sendTx(wallet, [
    {
      programAddress: PROGRAM_ID,
      accounts: [
        { address: wallet.address, role: AccountRole.WRITABLE_SIGNER, signer: wallet },
        { address: config, role: AccountRole.WRITABLE },
        { address: treasury, role: AccountRole.WRITABLE },
        { address: bet, role: AccountRole.WRITABLE },
        { address: SYSTEM_PROGRAM, role: AccountRole.READONLY },
      ],
      data: concat(disc("place_bet"), u64Le(nonce), u64Le(amount), new Uint8Array([choice])),
    },
  ]);
  console.log(`placed   https://explorer.solana.com/tx/${placeSig}?cluster=devnet`);

  // Bet layout: 8 discriminator | 32 player | 8 amount | 1 choice | 8 target_slot.
  const betData = (await fetchAccountData(bet))!;
  const targetSlot = new DataView(betData.buffer, betData.byteOffset).getBigUint64(49, true);

  // --- wait for the deciding slot, then settle ------------------------------
  process.stdout.write(`waiting for slot ${targetSlot} to land`);
  while ((await rpc.getSlot({ commitment: "confirmed" }).send()) <= targetSlot) {
    process.stdout.write(".");
    await sleep(400);
  }
  console.log();

  const balanceBefore = await getBalance(wallet.address);
  const settleIx: Ix = {
    programAddress: PROGRAM_ID,
    accounts: [
      { address: config, role: AccountRole.WRITABLE },
      { address: treasury, role: AccountRole.WRITABLE },
      { address: bet, role: AccountRole.WRITABLE },
      { address: wallet.address, role: AccountRole.WRITABLE },
      { address: SLOT_HASHES_SYSVAR, role: AccountRole.READONLY },
      { address: SYSTEM_PROGRAM, role: AccountRole.READONLY },
    ],
    data: disc("settle_bet"),
  };

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
    const result = data[57] === 0 ? "heads" : "tails";
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
