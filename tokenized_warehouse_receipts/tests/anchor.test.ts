// No imports needed: web3, anchor, pg and more are globally available

declare function setTimeout(
  handler: (...args: any[]) => void,
  timeout?: number,
  ...args: any[]
): number;

// SPL helpers (Solana Playground exposes one of these)
const spl: any =
  (globalThis as any).splToken ??
  (globalThis as any).spl ??
  (() => {
    throw new Error("SPL Token helpers not found (splToken/spl).");
  })();

describe("tokenized_warehouse_receipts", () => {
  const connection = pg.connection;
  const wallet = pg.wallet; // use as the default signer via wallet adapter inside Playground
  const program = pg.program;

  // test actors (we control these keys, so we can sign CPIs explicitly)
  const warehouseAuthority = web3.Keypair.generate();
  const long = web3.Keypair.generate();
  const short = web3.Keypair.generate();
  const mintAuthority = web3.Keypair.generate(); // authority for both mints we create

  // SPL mints & ATAs
  let quoteMint: web3.PublicKey;
  let receiptMint: web3.PublicKey;
  let longQuoteAta: web3.PublicKey;
  let shortQuoteAta: web3.PublicKey;

  // PDAs
  let marketPda: web3.PublicKey, marketBump: number;
  let warehousePda: web3.PublicKey, warehouseBump: number;
  let receiptMintAuthPda: web3.PublicKey, receiptMintAuthBump: number;

  // a deal (cash)
  let dealPda: web3.PublicKey, dealBump: number;
  let vaultAuthPda: web3.PublicKey, vaultAuthBump: number;
  let longMarginVault: web3.PublicKey;
  let shortMarginVault: web3.PublicKey;
  let feeVault: web3.PublicKey;

  // constants
  const DECIMALS = 6;
  const FEE_BPS = 50; // u16 number (0.50%)
  const PRICE_EXPONENT = -6;

  // helpers
  async function airdrop(pubkey: web3.PublicKey, lamports = 1e9) {
    const sig = await connection.requestAirdrop(pubkey, lamports);
    await connection.confirmTransaction(sig, "confirmed");
  }
  function toUnitsBN(n: number): any {
    // convert a decimal to 6dp integer BN (safe for test ranges)
    return new BN(Math.round(n * 10 ** DECIMALS));
  }
  async function getTokenAmount(ata: web3.PublicKey): Promise<number> {
    const acc = await spl.getAccount(connection, ata);
    return Number(acc.amount);
  }
  async function sleep(ms: number) {
    await new Promise((r) => setTimeout(r, ms));
  }

  before("setup keys, fund signers, create mints/ATAs, PDAs", async () => {
    await airdrop(warehouseAuthority.publicKey);
    await airdrop(long.publicKey);
    await airdrop(short.publicKey);
    await airdrop(mintAuthority.publicKey);

    // Create quote & receipt mints (mintAuthority is the mint authority we control)
    quoteMint = await spl.createMint(
      connection,
      mintAuthority,                  // payer of fees
      mintAuthority.publicKey,        // mint authority
      null,                           // freeze authority
      DECIMALS
    );
    receiptMint = await spl.createMint(
      connection,
      mintAuthority,
      mintAuthority.publicKey,
      null,
      DECIMALS
    );

    // PDAs
    [marketPda, marketBump] = web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("market"),
        wallet.publicKey.toBuffer(),   // market authority = wallet.publicKey
        receiptMint.toBuffer(),
        quoteMint.toBuffer(),
      ],
      program.programId
    );
    [warehousePda, warehouseBump] = web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("warehouse"),
        marketPda.toBuffer(),
        warehouseAuthority.publicKey.toBuffer(),
      ],
      program.programId
    );
    [receiptMintAuthPda, receiptMintAuthBump] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("receipt_auth"), warehousePda.toBuffer()],
      program.programId
    );

    // Long/Short quote ATAs
    longQuoteAta = (
      await spl.getOrCreateAssociatedTokenAccount(
        connection,
        mintAuthority,         // payer
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

    // Mint 10,000 quote tokens to each
    await spl.mintTo(
      connection,
      mintAuthority,
      quoteMint,
      longQuoteAta,
      mintAuthority, // authority
      Math.round(10_000 * 10 ** DECIMALS)
    );
    await spl.mintTo(
      connection,
      mintAuthority,
      quoteMint,
      shortQuoteAta,
      mintAuthority,
      Math.round(10_000 * 10 ** DECIMALS)
    );
  });

  it("init_market", async () => {
    // fee_bps is u16 -> pass a number (NOT BN)
    const tx = await program.methods
      .initMarket(FEE_BPS, wallet.publicKey /* oracle_authority */)
      .accounts({
        authority: wallet.publicKey,
        quoteMint,
        receiptMint,
        market: marketPda,
        systemProgram: web3.SystemProgram.programId,
      })
      // wallet signs implicitly via Playground; don't pass .signers([...wallet...])
      .rpc();

    await connection.confirmTransaction(tx, "confirmed");

    const market = await program.account.market.fetch(marketPda);
    assert.equal(market.version, 1);
    assert.equal(new web3.PublicKey(market.authority).toBase58(), wallet.publicKey.toBase58());
    assert.equal(market.feeBps, FEE_BPS);
    assert.equal(market.isPaused, false);
  });

  it("post_price (by oracle/authority)", async () => {
    const priceBN = toUnitsBN(123.45); // u64 BN
    const settleTs = Math.floor(Date.now() / 1000) + 3600; // i64 (pass BN)

    const tx = await program.methods
      .postPrice(priceBN, PRICE_EXPONENT, new BN(settleTs))
      .accounts({
        market: marketPda,
        poster: wallet.publicKey, // allowed (oracleAuthority == wallet.publicKey)
        quoteMint,
        receiptMint,
      })
      .rpc();

    await connection.confirmTransaction(tx, "confirmed");
    const market = await program.account.market.fetch(marketPda);
    assert.equal(market.lastPrice.toString(), priceBN.toString());
    assert.equal(market.priceExponent, PRICE_EXPONENT);
    assert.equal(Number(market.settleTs), settleTs);
  });

  it("init_warehouse (and transfer mint authority to PDA)", async () => {
    const tx = await program.methods
      .initWarehouse()
      .accounts({
        warehouseAuthority: warehouseAuthority.publicKey,
        market: marketPda,
        authority: wallet.publicKey, // equality check only
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
    const wh = await program.account.warehouse.fetch(warehousePda);
    assert.equal(new web3.PublicKey(wh.market).toBase58(), marketPda.toBase58());
    assert.equal(new web3.PublicKey(wh.receiptMint).toBase58(), receiptMint.toBase58());
    assert.equal(wh.bump, receiptMintAuthBump);
  });

  it("mint_receipt (warehouse authority via PDA mint authority)", async () => {
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

    const mintAmount = Math.round(100 * 10 ** DECIMALS);
    const tx = await program.methods
      .mintReceipt(new BN(mintAmount))
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
    assert.equal(bal, mintAmount);
  });

  it("open_deal (cash) + deposit_margin + settle_cash", async () => {
    // Derive deal/vault_auth PDAs
    [dealPda, dealBump] = web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("deal"),
        marketPda.toBuffer(),
        long.publicKey.toBuffer(),
        short.publicKey.toBuffer(),
      ],
      program.programId
    );
    [vaultAuthPda, vaultAuthBump] = web3.PublicKey.findProgramAddressSync(
      [Buffer.from("vault_auth"), dealPda.toBuffer()],
      program.programId
    );

    // vault ATAs owned by vault_auth
    longMarginVault = spl.getAssociatedTokenAddressSync(quoteMint, vaultAuthPda, true);
    shortMarginVault = spl.getAssociatedTokenAddressSync(quoteMint, vaultAuthPda, true);
    feeVault = spl.getAssociatedTokenAddressSync(quoteMint, marketPda, true);

    // params
    const dealId = new BN(1);               // u64
    const strike = toUnitsBN(100);          // u64
    const qty = toUnitsBN(10);              // u64
    const settleTs = new BN(Math.floor(Date.now() / 1000) + 2); // i64 BN
    const settlementKind = { cash: {} };    // IDL enum object
    const initMarginLong = toUnitsBN(2000); // u64
    const initMarginShort = toUnitsBN(2000);

    // open_deal
    let tx = await program.methods
      .openDeal(dealId, strike, qty, settleTs, settlementKind, initMarginLong, initMarginShort)
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

    // deposit extra margin (both sides)
    const addLong = toUnitsBN(100);
    const addShort = toUnitsBN(200);

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

    // wait for settle_ts
    await sleep(2500);

    const longReceiveQuoteAta = longQuoteAta;
    const shortReceiveQuoteAta = spl.getAssociatedTokenAddressSync(quoteMint, short.publicKey);

    const preLongRecv = await getTokenAmount(longReceiveQuoteAta);
    const preShortRecv = await getTokenAmount(shortReceiveQuoteAta);
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

    const postLongRecv = await getTokenAmount(longReceiveQuoteAta);
    const postShortRecv = await getTokenAmount(shortReceiveQuoteAta);
    const postFee = await getTokenAmount(feeVault);
    const postLM = await getTokenAmount(longMarginVault);
    const postSM = await getTokenAmount(shortMarginVault);

    // margins are returned (vaults drained)
    assert.equal(postLM, 0);
    assert.equal(postSM, 0);

    // winner got something, fee collected (exact math depends on residual returns)
    assert.equal(postLongRecv > preLongRecv, true);
    assert.equal(postFee > preFee, true);

    const deal = await program.account.deal.fetch(dealPda);
    assert.equal(deal.isSettled, true);
  });

  it("settle_physical (separate deal) - happy path", async () => {
    // New participants to avoid PDA collision for ["deal", market, long, short]
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

    // Give short2 the receipts to deliver
    const short2ReceiptAta = (
      await spl.getOrCreateAssociatedTokenAccount(
        connection,
        mintAuthority,
        receiptMint,
        short2.publicKey
      )
    ).address;
    await spl.mintTo(
      connection,
      mintAuthority,
      receiptMint,
      short2ReceiptAta,
      mintAuthority,
      Math.round(5 * 10 ** DECIMALS)
    );

    const long2ReceiptAta = (
      await spl.getOrCreateAssociatedTokenAccount(
        connection,
        mintAuthority,
        receiptMint,
        long2.publicKey
      )
    ).address;

    // new deal PDAs
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
    const dealId = new BN(2);
    const strike = toUnitsBN(50);
    const qty = toUnitsBN(5);
    const settleTs = new BN(Math.floor(Date.now() / 1000) + 2);
    const settlementKind = { physical: {} };
    const initMarginLong = toUnitsBN(1000);
    const initMarginShort = toUnitsBN(1000);

    // open physical deal
    let tx = await program.methods
      .openDeal(dealId, strike, qty, settleTs, settlementKind, initMarginLong, initMarginShort)
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

    await sleep(2500); // wait until settle_ts

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

    // receipts moved from short2 to long2
    const long2Receipts = await getTokenAmount(long2ReceiptAta);
    assert.equal(long2Receipts, Math.round(5 * 10 ** DECIMALS));

    const d2 = await program.account.deal.fetch(deal2Pda);
    assert.equal(d2.isSettled, true);
  });
});
