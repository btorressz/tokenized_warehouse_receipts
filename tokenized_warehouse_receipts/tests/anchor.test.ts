// test file
// // No imports needed: web3, anchor, pg and more are globally available
// Key changes covered:
// - init_market(authority, fee_bps, oracle_authority, governance_authority,
//   base_initial_margin_bps, maintenance_margin_bps, vol_multiplier_bps)
// - post_price(price:u64, exponent:i32, settle_ts:i64, vol_bps:u16) with accounts {market, poster}
// - open_deal(..., deal_version:u8, ...) requires DEAL_VERSION=1 and allowed collateral check
// - dynamic initial margin (required_initial_margin) enforced
// - deposit_margin(side enum object)
// - settle_cash / settle_physical / settle_partial_physical
// - cross-margin (cm_create, cm_deposit, cm_withdraw, cm_move_to_deal, cm_move_from_deal)
// - freeze/unfreeze guards (light touch via happy-path usage)
//
// Assumes globals: web3, anchor, pg, BN, assert
// Tries both `splToken` and `spl` for SPL helpers.

declare function setTimeout(
  handler: (...args: any[]) => void,
  timeout?: number,
  ...args: any[]
): number;

// SPL helpers resolution
const spl: any =
  (globalThis as any).splToken ??
  (globalThis as any).spl ??
  (() => {
    throw new Error("SPL Token helpers not found (splToken/spl).");
  })();

describe("tokenized_warehouse_receipts ()", () => {
  const connection = pg.connection;
  const wallet = pg.wallet;
  const program = pg.program;

  // actors
  const warehouseAuthority = web3.Keypair.generate();
  const long = web3.Keypair.generate();
  const short = web3.Keypair.generate();
  const mintAuthority = web3.Keypair.generate();

  // mints & ATAs
  let quoteMint: web3.PublicKey;
  let receiptMint: web3.PublicKey;
  let longQuoteAta: web3.PublicKey;
  let shortQuoteAta: web3.PublicKey;

  // PDAs
  let marketPda: web3.PublicKey;
  let warehousePda: web3.PublicKey;
  let receiptMintAuthPda: web3.PublicKey;
  let dealPda: web3.PublicKey;
  let vaultAuthPda: web3.PublicKey;
  let longMarginVault: web3.PublicKey;
  let shortMarginVault: web3.PublicKey;
  let feeVault: web3.PublicKey;

  // cross-margin
  let cmPda: web3.PublicKey;
  let cmVaultAuthPda: web3.PublicKey;
  let cmVaultAta: web3.PublicKey;

  // constants
  const DECIMALS = 6;
  const PRICE_EXPONENT = -6;

  // market params (new)
  const FEE_BPS = 50; // <= 1000
  const BASE_IM_BPS = 500; // 5% base initial margin
  const MAINT_BPS = 300; // 3% maintenance (not enforced in tests but set)
  const VOL_MULT_BPS = 200; // scales vol → extra margin
  const ORACLE = () => wallet.publicKey; // poster == oracle or authority or governance
  const GOVERNANCE = () => wallet.publicKey;

  // helpers
  async function airdrop(pubkey: web3.PublicKey, lamports = 1e9) {
    const sig = await connection.requestAirdrop(pubkey, lamports);
    await connection.confirmTransaction(sig, "confirmed");
  }
  function toUnitsBN(n: number): any {
    return new BN(Math.round(n * 10 ** DECIMALS));
  }
  async function getTokenAmount(ata: web3.PublicKey): Promise<number> {
    const acc = await spl.getAccount(connection, ata);
    return Number(acc.amount);
  }
  async function sleep(ms: number) {
    await new Promise((r) => setTimeout(r, ms));
  }

  // mirrors Rust required_initial_margin for tests
  function pow10u128(p: number): BN {
    return new BN(10).pow(new BN(p));
  }
  function requiredInitialMargin(
    priceExponent: number,
    baseImBps: number,
    volMultBps: number,
    lastVolBps: number,
    strikePriceU64: BN, // scaled by price exponent (e.g., 6)
    qtyU64: BN // mint decimals
  ): BN {
    // notional = strike * qty / 10^abs(exp)
    const denom = pow10u128(Math.abs(priceExponent));
    const notional = strikePriceU64.mul(qtyU64).div(denom);
    const volAdjBps = Math.floor((volMultBps * lastVolBps) / 10000);
    const totalBps = baseImBps + volAdjBps;
    return notional.mul(new BN(totalBps)).div(new BN(10000));
  }

  before("setup: fund signers, create mints/ATAs, derive PDAs", async () => {
    await airdrop(warehouseAuthority.publicKey);
    await airdrop(long.publicKey);
    await airdrop(short.publicKey);
    await airdrop(mintAuthority.publicKey);

    // Create quote & receipt mints
    quoteMint = await spl.createMint(
      connection,
      mintAuthority,
      mintAuthority.publicKey,
      null,
      DECIMALS
    );
    receiptMint = await spl.createMint(
      connection,
      mintAuthority,
      mintAuthority.publicKey,
      null,
      DECIMALS
    );

    // Market PDA
    [marketPda] = web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("market"),
        wallet.publicKey.toBuffer(),
        receiptMint.toBuffer(),
        quoteMint.toBuffer(),
      ],
      program.programId
    );

    // Warehouse PDAs
    [warehousePda] = web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("warehouse"),
        marketPda.toBuffer(),
        warehouseAuthority.publicKey.toBuffer(),
      ],
      program.programId
    );
    [receiptMintAuthPda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("receipt_auth"), warehousePda.toBuffer()],
      program.programId
    );

    // ATAs for long/short
    longQuoteAta = (
      await spl.getOrCreateAssociatedTokenAccount(
        connection,
        mintAuthority,
        quoteMint,
        long.publicKey
      )
    ).address;
    shortQuoteAta = (
      await spl.getOrCreateAssociatedTokenAccount(
        connection,
        mintAuthority,
        quoteMint,
        short.publicKey
      )
    ).address;

    // fund quote
    await spl.mintTo(
      connection,
      mintAuthority,
      quoteMint,
      longQuoteAta,
      mintAuthority,
      Math.round(50_000 * 10 ** DECIMALS)
    );
    await spl.mintTo(
      connection,
      mintAuthority,
      quoteMint,
      shortQuoteAta,
      mintAuthority,
      Math.round(50_000 * 10 ** DECIMALS)
    );
  });

  it("init_market (with governance & margin params)", async () => {
    const tx = await program.methods
      .initMarket(
        FEE_BPS,
        ORACLE(),
        GOVERNANCE(),
        BASE_IM_BPS,
        MAINT_BPS,
        VOL_MULT_BPS
      )
      .accounts({
        authority: wallet.publicKey,
        quoteMint,
        receiptMint,
        market: marketPda,
        systemProgram: web3.SystemProgram.programId,
      })
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    const m = await program.account.market.fetch(marketPda);
    assert.equal(m.feeBps, FEE_BPS);
    assert.equal(m.baseInitialMarginBps, BASE_IM_BPS);
    assert.equal(m.volMultiplierBps, VOL_MULT_BPS);
    assert.equal(new web3.PublicKey(m.authority).toBase58(), wallet.publicKey.toBase58());
  });

  it("post_price (with vol_bps)", async () => {
    const priceBN = toUnitsBN(120.0);
    const settleTs = Math.floor(Date.now() / 1000) + 3600;
    const volBps = 500; // 5% volatility reading

    const tx = await program.methods
      .postPrice(priceBN, PRICE_EXPONENT, new BN(settleTs), volBps)
      .accounts({
        market: marketPda,
        poster: wallet.publicKey,
      })
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    const m = await program.account.market.fetch(marketPda);
    assert.equal(m.lastPrice.toString(), priceBN.toString());
    assert.equal(m.priceExponent, PRICE_EXPONENT);
    assert.equal(m.lastVolBps, volBps);
  });

  it("init_warehouse → mint_receipt", async () => {
    // init_warehouse
    let tx = await program.methods
      .initWarehouse()
      .accounts({
        warehouseAuthority: warehouseAuthority.publicKey,
        market: marketPda,
        authority: wallet.publicKey,
        quoteMint,
        receiptMint,
        receiptMintAuth: receiptMintAuthPda,
        warehouse: warehousePda,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        systemProgram: web3.SystemProgram.programId,
      })
      .signers([warehouseAuthority])
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    // mint_receipt to a recipient
    const recipient = web3.Keypair.generate();
    await airdrop(recipient.publicKey);
    const toReceiptAta = (
      await spl.getOrCreateAssociatedTokenAccount(
        connection,
        mintAuthority,
        receiptMint,
        recipient.publicKey
      )
    ).address;

    const amount = Math.round(250 * 10 ** DECIMALS);
    tx = await program.methods
      .mintReceipt(new BN(amount))
      .accounts({
        warehouse: warehousePda,
        market: marketPda,
        receiptMint,
        receiptMintAuth: receiptMintAuthPda,
        toReceiptAta,
        warehouseAuthority: warehouseAuthority.publicKey,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
      })
      .signers([warehouseAuthority])
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    const bal = await getTokenAmount(toReceiptAta);
    assert.equal(bal, amount);
  });

  it("open_deal (cash) with required initial margin → deposit_margin → settle_cash", async () => {
    // PDAs for deal
    [dealPda] = web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("deal"),
        marketPda.toBuffer(),
        long.publicKey.toBuffer(),
        short.publicKey.toBuffer(),
      ],
      program.programId
    );
    [vaultAuthPda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("vault_auth"), dealPda.toBuffer()],
      program.programId
    );

    // ATA addresses (PDA-owned)
    longMarginVault = spl.getAssociatedTokenAddressSync(quoteMint, vaultAuthPda, true);
    shortMarginVault = spl.getAssociatedTokenAddressSync(quoteMint, vaultAuthPda, true);
    feeVault = spl.getAssociatedTokenAddressSync(quoteMint, marketPda, true);

    // params
    const DEAL_VERSION = 1; // must match on-chain
    const dealId = new BN(101);
    const strike = toUnitsBN(100); // 100 quote per receipt unit
    const qty = toUnitsBN(8);      // 8 receipts
    const settleTs = new BN(Math.floor(Date.now() / 1000) + 2);
    const settlementKind = { cash: {} };

    // compute required IM using market snapshot (after last post_price)
    const m = await program.account.market.fetch(marketPda);
    const reqIM = requiredInitialMargin(
      m.priceExponent,
      m.baseInitialMarginBps,
      m.volMultiplierBps,
      m.lastVolBps,
      new BN(strike),
      new BN(qty)
    );

    // pad a bit above required to be safe
    const imLong = reqIM.add(new BN(Math.round(0.1 * 10 ** DECIMALS)));  // +0.1 tokens
    const imShort = reqIM.add(new BN(Math.round(0.1 * 10 ** DECIMALS)));

    // open_deal
    let tx = await program.methods
      .openDeal(
        dealId,
        DEAL_VERSION,
        strike,
        qty,
        settleTs,
        settlementKind,
        imLong,
        imShort
      )
      .accounts({
        market: marketPda,
        long: long.publicKey,
        short: short.publicKey,
        quoteMint,
        longQuoteAta,
        shortQuoteAta,
        deal: dealPda,
        longMarginVault,
        shortMarginVault,
        vaultAuth: vaultAuthPda,
        feeVault,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        associatedTokenProgram: spl.ASSOCIATED_TOKEN_PROGRAM_ID,
        systemProgram: web3.SystemProgram.programId,
      })
      .signers([long, short])
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    // extra deposits
    const addLong = toUnitsBN(1.0);
    const addShort = toUnitsBN(1.5);

    tx = await program.methods
      .depositMargin({ long: {} }, addLong)
      .accounts({
        deal: dealPda,
        quoteMint,
        payer: long.publicKey,
        payerQuoteAta: longQuoteAta,
        vaultAuth: vaultAuthPda,
        longMarginVault,
        shortMarginVault,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        associatedTokenProgram: spl.ASSOCIATED_TOKEN_PROGRAM_ID,
      })
      .signers([long])
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    tx = await program.methods
      .depositMargin({ short: {} }, addShort)
      .accounts({
        deal: dealPda,
        quoteMint,
        payer: short.publicKey,
        payerQuoteAta: shortQuoteAta,
        vaultAuth: vaultAuthPda,
        longMarginVault,
        shortMarginVault,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        associatedTokenProgram: spl.ASSOCIATED_TOKEN_PROGRAM_ID,
      })
      .signers([short])
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    // wait to pass settle_ts
    await sleep(2500);

    const longReceiveQuoteAta = longQuoteAta;
    const shortReceiveQuoteAta = spl.getAssociatedTokenAddressSync(quoteMint, short.publicKey);

    const preLong = await getTokenAmount(longReceiveQuoteAta);
    const preShort = await getTokenAmount(shortReceiveQuoteAta);
    const preFee = await getTokenAmount(feeVault);

    tx = await program.methods
      .settleCash()
      .accounts({
        market: marketPda,
        deal: dealPda,
        quoteMint,
        receiptMint,
        vaultAuth: vaultAuthPda,
        longMarginVault,
        shortMarginVault,
        longReceiveQuoteAta,
        shortReceiveQuoteAta,
        feeVault,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        associatedTokenProgram: spl.ASSOCIATED_TOKEN_PROGRAM_ID,
      })
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    const postLong = await getTokenAmount(longReceiveQuoteAta);
    const postShort = await getTokenAmount(shortReceiveQuoteAta);
    const postFee = await getTokenAmount(feeVault);
    const postLM = await getTokenAmount(longMarginVault);
    const postSM = await getTokenAmount(shortMarginVault);

    // vaults drained after settlement
    assert.equal(postLM, 0);
    assert.equal(postSM, 0);
    // someone won and fee accrued
    assert.equal(postFee > preFee, true);
    assert.equal(postLong !== preLong || postShort !== preShort, true);

    const d = await program.account.deal.fetch(dealPda);
    assert.equal(d.isSettled, true);
  });

  it("settle_physical and settle_partial_physical", async () => {
    // new pair to avoid PDA collision
    const long2 = web3.Keypair.generate();
    const short2 = web3.Keypair.generate();
    await airdrop(long2.publicKey);
    await airdrop(short2.publicKey);

    const long2QuoteAta = (
      await spl.getOrCreateAssociatedTokenAccount(
        connection,
        mintAuthority,
        quoteMint,
        long2.publicKey
      )
    ).address;
    const short2QuoteAta = (
      await spl.getOrCreateAssociatedTokenAccount(
        connection,
        mintAuthority,
        quoteMint,
        short2.publicKey
      )
    ).address;

    const long2ReceiptAta = (
      await spl.getOrCreateAssociatedTokenAccount(
        connection,
        mintAuthority,
        receiptMint,
        long2.publicKey
      )
    ).address;
    const short2ReceiptAta = (
      await spl.getOrCreateAssociatedTokenAccount(
        connection,
        mintAuthority,
        receiptMint,
        short2.publicKey
      )
    ).address;

    // give short2 receipts to deliver
    await spl.mintTo(
      connection,
      mintAuthority,
      receiptMint,
      short2ReceiptAta,
      mintAuthority,
      Math.round(6 * 10 ** DECIMALS)
    );

    // PDAs
    const [deal2Pda] = web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("deal"),
        marketPda.toBuffer(),
        long2.publicKey.toBuffer(),
        short2.publicKey.toBuffer(),
      ],
      program.programId
    );
    const [vaultAuth2Pda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("vault_auth"), deal2Pda.toBuffer()],
      program.programId
    );
    const long2MarginVault = spl.getAssociatedTokenAddressSync(quoteMint, vaultAuth2Pda, true);
    const short2MarginVault = spl.getAssociatedTokenAddressSync(quoteMint, vaultAuth2Pda, true);

    // params
    const DEAL_VERSION = 1;
    const dealId = new BN(202);
    const strike = toUnitsBN(50);
    const qty = toUnitsBN(6); // we will partially settle first
    const settleTs = new BN(Math.floor(Date.now() / 1000) + 2);
    const settlementKind = { physical: {} };

    // compute required IM (using last posted price/vol)
    const m = await program.account.market.fetch(marketPda);
    const reqIM = requiredInitialMargin(
      m.priceExponent,
      m.baseInitialMarginBps,
      m.volMultiplierBps,
      m.lastVolBps,
      new BN(strike),
      new BN(qty)
    );
    const imPad = new BN(Math.round(0.1 * 10 ** DECIMALS));
    const imLong = reqIM.add(imPad);
    const imShort = reqIM.add(imPad);

    // open physical deal
    let tx = await program.methods
      .openDeal(
        dealId,
        DEAL_VERSION,
        strike,
        qty,
        settleTs,
        settlementKind,
        imLong,
        imShort
      )
      .accounts({
        market: marketPda,
        long: long2.publicKey,
        short: short2.publicKey,
        quoteMint,
        longQuoteAta: long2QuoteAta,
        shortQuoteAta: short2QuoteAta,
        deal: deal2Pda,
        longMarginVault: long2MarginVault,
        shortMarginVault: short2MarginVault,
        vaultAuth: vaultAuth2Pda,
        feeVault,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        associatedTokenProgram: spl.ASSOCIATED_TOKEN_PROGRAM_ID,
        systemProgram: web3.SystemProgram.programId,
      })
      .signers([long2, short2])
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    // wait for settle time
    await sleep(2500);

    // PARTIAL settlement for 2 receipts first
    const partialAmount = Math.round(2 * 10 ** DECIMALS);
    tx = await program.methods
      .settlePartialPhysical(new BN(partialAmount))
      .accounts({
        deal: deal2Pda,
        market: marketPda,
        quoteMint,
        receiptMint,
        vaultAuth: vaultAuth2Pda,
        longMarginVault: long2MarginVault,
        shortMarginVault: short2MarginVault,
        long: long2.publicKey,
        short: short2.publicKey,
        longReceiptAta: long2ReceiptAta,
        shortReceiptAta: short2ReceiptAta,
        longReceiveQuoteAta: long2QuoteAta,
        shortReceiveQuoteAta: short2QuoteAta,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        associatedTokenProgram: spl.ASSOCIATED_TOKEN_PROGRAM_ID,
      })
      .signers([long2, short2])
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    // FULL settlement for remaining (4 receipts)
    tx = await program.methods
      .settlePhysical()
      .accounts({
        deal: deal2Pda,
        market: marketPda,
        quoteMint,
        receiptMint,
        vaultAuth: vaultAuth2Pda,
        longMarginVault: long2MarginVault,
        shortMarginVault: short2MarginVault,
        long: long2.publicKey,
        short: short2.publicKey,
        longReceiptAta: long2ReceiptAta,
        shortReceiptAta: short2ReceiptAta,
        longReceiveQuoteAta: long2QuoteAta,
        shortReceiveQuoteAta: short2QuoteAta,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        associatedTokenProgram: spl.ASSOCIATED_TOKEN_PROGRAM_ID,
      })
      .signers([long2, short2])
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    // margin vaults drained
    const postLM = await getTokenAmount(long2MarginVault);
    const postSM = await getTokenAmount(short2MarginVault);
    assert.equal(postLM, 0);
    assert.equal(postSM, 0);

    const d2 = await program.account.deal.fetch(deal2Pda);
    assert.equal(d2.isSettled, true);
    assert.equal(Number(d2.qtyReceiptAmount), 0);
  });

  it("cross-margin: cm_create → cm_deposit → cm_move_to_deal → cm_move_from_deal → cm_withdraw", async () => {
    const owner = long; // reuse long as cross-margin owner
    // derive cross-margin PDA
    [cmPda] = web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("cross_margin"),
        marketPda.toBuffer(),
        owner.publicKey.toBuffer(),
        quoteMint.toBuffer(),
      ],
      program.programId
    );
    [cmVaultAuthPda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("cm_vault_auth"), cmPda.toBuffer()],
      program.programId
    );
    cmVaultAta = spl.getAssociatedTokenAddressSync(quoteMint, cmVaultAuthPda, true);

    // create
    let tx = await program.methods
      .cmCreate()
      .accounts({
        owner: owner.publicKey,
        market: marketPda,
        quoteMint,
        crossMargin: cmPda,
        cmVaultAuth: cmVaultAuthPda,
        cmVaultAta,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        associatedTokenProgram: spl.ASSOCIATED_TOKEN_PROGRAM_ID,
        systemProgram: web3.SystemProgram.programId,
      })
      .signers([owner])
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    // deposit into cross margin
    const deposit = toUnitsBN(50);
    tx = await program.methods
      .cmDeposit(new BN(deposit))
      .accounts({
        owner: owner.publicKey,
        market: marketPda,
        quoteMint,
        crossMargin: cmPda,
        cmVaultAuth: cmVaultAuthPda,
        cmVaultAta,
        ownerQuoteAta: longQuoteAta,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        associatedTokenProgram: spl.ASSOCIATED_TOKEN_PROGRAM_ID,
      })
      .signers([owner])
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    // move from CM to an existing (settled) deal's long vault just to exercise path
    // (create a tiny fresh deal to avoid interacting with settled one)
    const tempShort = web3.Keypair.generate();
    await airdrop(tempShort.publicKey);
    const tempShortQuoteAta = (
      await spl.getOrCreateAssociatedTokenAccount(
        connection,
        mintAuthority,
        quoteMint,
        tempShort.publicKey
      )
    ).address;

    const [deal3Pda] = web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("deal"),
        marketPda.toBuffer(),
        owner.publicKey.toBuffer(),
        tempShort.publicKey.toBuffer(),
      ],
      program.programId
    );
    const [vaultAuth3Pda] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("vault_auth"), deal3Pda.toBuffer()],
      program.programId
    );
    const long3MarginVault = spl.getAssociatedTokenAddressSync(quoteMint, vaultAuth3Pda, true);
    const short3MarginVault = spl.getAssociatedTokenAddressSync(quoteMint, vaultAuth3Pda, true);

    const dId = new BN(303);
    const strike = toUnitsBN(90);
    const qty = toUnitsBN(1);
    const settleTs = new BN(Math.floor(Date.now() / 1000) + 5);
    const settlementKind = { cash: {} };

    // compute IM & open small deal
    const m = await program.account.market.fetch(marketPda);
    const reqIM = requiredInitialMargin(
      m.priceExponent,
      m.baseInitialMarginBps,
      m.volMultiplierBps,
      m.lastVolBps,
      new BN(strike),
      new BN(qty)
    ).add(new BN(1)); // nudge

    tx = await program.methods
      .openDeal(
        dId,
        1,
        strike,
        qty,
        settleTs,
        settlementKind,
        reqIM,
        reqIM
      )
      .accounts({
        market: marketPda,
        long: owner.publicKey,
        short: tempShort.publicKey,
        quoteMint,
        longQuoteAta: longQuoteAta,
        shortQuoteAta: tempShortQuoteAta,
        deal: deal3Pda,
        longMarginVault: long3MarginVault,
        shortMarginVault: short3MarginVault,
        vaultAuth: vaultAuth3Pda,
        feeVault,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        associatedTokenProgram: spl.ASSOCIATED_TOKEN_PROGRAM_ID,
        systemProgram: web3.SystemProgram.programId,
      })
      .signers([owner, tempShort])
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    // move 10 tokens from CM → deal long vault
    const moveAmt = toUnitsBN(10);
    tx = await program.methods
      .cmMoveToDeal({ long: {} }, new BN(moveAmt))
      .accounts({
        owner: owner.publicKey,
        market: marketPda,
        quoteMint,
        deal: deal3Pda,
        vaultAuth: vaultAuth3Pda,
        crossMargin: cmPda,
        cmVaultAuth: cmVaultAuthPda,
        cmVaultAta,
        longMarginVault: long3MarginVault,
        shortMarginVault: short3MarginVault,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
      })
      .signers([owner])
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    // move 5 back from deal long vault → CM
    const moveBack = toUnitsBN(5);
    tx = await program.methods
      .cmMoveFromDeal({ long: {} }, new BN(moveBack))
      .accounts({
        owner: owner.publicKey,
        market: marketPda,
        quoteMint,
        deal: deal3Pda,
        vaultAuth: vaultAuth3Pda,
        crossMargin: cmPda,
        cmVaultAuth: cmVaultAuthPda,
        cmVaultAta,
        longMarginVault: long3MarginVault,
        shortMarginVault: short3MarginVault,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
      })
      .signers([owner])
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    // withdraw from CM back to owner
    const withdraw = toUnitsBN(3);
    tx = await program.methods
      .cmWithdraw(new BN(withdraw))
      .accounts({
        owner: owner.publicKey,
        market: marketPda,
        quoteMint,
        crossMargin: cmPda,
        cmVaultAuth: cmVaultAuthPda,
        cmVaultAta,
        ownerQuoteAta: longQuoteAta,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        associatedTokenProgram: spl.ASSOCIATED_TOKEN_PROGRAM_ID,
      })
      .signers([owner])
      .rpc();
    await connection.confirmTransaction(tx, "confirmed");

    // sanity: CM vault still has funds (>0)
    const cmBal = await getTokenAmount(cmVaultAta);
    assert.equal(cmBal > 0, true);
  });
});

