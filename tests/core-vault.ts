import * as anchor from "@coral-xyz/anchor";
import {
  PublicKey,
  Keypair,
  SystemProgram,
  Transaction,
  TransactionInstruction,
} from "@solana/web3.js";
import { assert } from "chai";
import * as crypto from "crypto";
import * as fs from "fs";
import * as path from "path";

// `setl8-shared-interfaces` v0.2.0 has no anchor-lang dependency at all (by
// design — see its Cargo.toml comments), so `ChallengeSize` can't implement
// anchor_lang's `IdlBuild` trait without either that crate or core-vault
// taking on a dependency the other side doesn't want, and Rust's orphan
// rule blocks implementing a foreign trait for a foreign type from a third
// crate (core-vault) either way. `anchor build`'s default `idl-build`
// feature fails on this (confirmed while building Module 1 — flagged for
// your review, not silently worked around). Building with `--no-idl`
// avoids it, which means no generated `target/idl`/`target/types` client
// here — instructions are constructed by hand below instead, using the
// same raw discriminator + Borsh-arg-layout approach
// `setl8-shared-interfaces`'s own Rust builders use.

function disc(name: string): Buffer {
  return crypto.createHash("sha256").update(`global:${name}`).digest().subarray(0, 8);
}

function u16LE(n: number): Buffer {
  const b = Buffer.alloc(2);
  b.writeUInt16LE(n);
  return b;
}

function u64LE(n: number | bigint): Buffer {
  const b = Buffer.alloc(8);
  b.writeBigUInt64LE(BigInt(n));
  return b;
}

interface ChallengeSize {
  size: number | bigint;
  cost: number | bigint;
}

// Borsh's Vec<T> length prefix is a u32 (4 bytes).
function vecLenLE(n: number): Buffer {
  const b = Buffer.alloc(4);
  b.writeUInt32LE(n);
  return b;
}

function encodeChallengeSizes(sizes: ChallengeSize[]): Buffer {
  return Buffer.concat([
    vecLenLE(sizes.length),
    ...sizes.map((s) => Buffer.concat([u64LE(s.size), u64LE(s.cost)])),
  ]);
}

function loadKeypair(filePath: string): Keypair {
  const secret = JSON.parse(fs.readFileSync(filePath, "utf-8"));
  return Keypair.fromSecretKey(Uint8Array.from(secret));
}

describe("core-vault: Module 1 — ProductRegistry + CPI Gateway", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const connection = provider.connection;

  const programId = new PublicKey(
    fs.readFileSync(path.join(__dirname, "../Anchor.toml"), "utf-8").match(/core_vault = "([^"]+)"/)![1]
  );

  const sl8Admin = loadKeypair(path.join(__dirname, "fixtures/sl8-admin.json"));
  const rovAdmin = loadKeypair(path.join(__dirname, "fixtures/rov-admin.json"));

  function productRegistryPda(productProgramId: PublicKey): [PublicKey, number] {
    return PublicKey.findProgramAddressSync(
      [Buffer.from("product_registry"), productProgramId.toBuffer()],
      programId
    );
  }

  function registerProductIx(
    productProgramId: PublicKey,
    feeSplitBps: number,
    challengeSizes: ChallengeSize[],
    maxPayoutCount: number,
    productRegistry: PublicKey
  ): TransactionInstruction {
    const data = Buffer.concat([
      disc("register_product"),
      productProgramId.toBuffer(),
      u16LE(feeSplitBps),
      encodeChallengeSizes(challengeSizes),
      u64LE(maxPayoutCount),
    ]);
    return new TransactionInstruction({
      programId,
      keys: [
        { pubkey: sl8Admin.publicKey, isSigner: true, isWritable: true },
        { pubkey: rovAdmin.publicKey, isSigner: true, isWritable: false },
        { pubkey: productRegistry, isSigner: false, isWritable: true },
        { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
      ],
      data,
    });
  }

  function reactivateProductIx(productProgramId: PublicKey, productRegistry: PublicKey): TransactionInstruction {
    const data = Buffer.concat([disc("reactivate_product"), productProgramId.toBuffer()]);
    return new TransactionInstruction({
      programId,
      keys: [
        { pubkey: sl8Admin.publicKey, isSigner: true, isWritable: false },
        { pubkey: rovAdmin.publicKey, isSigner: true, isWritable: false },
        { pubkey: productRegistry, isSigner: false, isWritable: true },
      ],
      data,
    });
  }

  function updateProductConfigIx(
    productProgramId: PublicKey,
    challengeSizes: ChallengeSize[],
    feeSplitBps: number,
    maxPayoutCount: number,
    productRegistry: PublicKey
  ): TransactionInstruction {
    const data = Buffer.concat([
      disc("update_product_config"),
      productProgramId.toBuffer(),
      encodeChallengeSizes(challengeSizes),
      u16LE(feeSplitBps),
      u64LE(maxPayoutCount),
    ]);
    return new TransactionInstruction({
      programId,
      keys: [
        { pubkey: sl8Admin.publicKey, isSigner: true, isWritable: false },
        { pubkey: rovAdmin.publicKey, isSigner: true, isWritable: false },
        { pubkey: productRegistry, isSigner: false, isWritable: true },
      ],
      data,
    });
  }

  // Mirrors ProductRegistry's field layout exactly (state/product_registry.rs).
  function decodeProductRegistry(data: Buffer) {
    let offset = 8; // Anchor account discriminator
    const productProgramId = new PublicKey(data.subarray(offset, offset + 32));
    offset += 32;
    const challengeSizesLen = data.readUInt32LE(offset);
    offset += 4;
    const challengeSizes: ChallengeSize[] = [];
    for (let i = 0; i < challengeSizesLen; i++) {
      const size = data.readBigUInt64LE(offset);
      const cost = data.readBigUInt64LE(offset + 8);
      challengeSizes.push({ size, cost });
      offset += 16;
    }
    const feeSplitBps = data.readUInt16LE(offset);
    offset += 2;
    const maxPayoutCount = data.readBigUInt64LE(offset);
    offset += 8;
    const active = data.readUInt8(offset) === 1;
    offset += 1;
    const totalRequestsEmitted = data.readBigUInt64LE(offset);
    offset += 8;
    const bump = data.readUInt8(offset);
    return { productProgramId, challengeSizes, feeSplitBps, maxPayoutCount, active, totalRequestsEmitted, bump };
  }

  async function send(ix: TransactionInstruction, signers: Keypair[]) {
    const tx = new Transaction().add(ix);
    return anchor.web3.sendAndConfirmTransaction(connection, tx, signers);
  }

  before(async () => {
    for (const kp of [sl8Admin, rovAdmin]) {
      const sig = await connection.requestAirdrop(kp.publicKey, 2_000_000_000);
      await connection.confirmTransaction(sig, "confirmed");
    }
  });

  it("computed discriminators match setl8-shared-interfaces' precomputed constants", () => {
    // Sanity-checks the exact-name-match assumption this whole integration
    // depends on, independent of the Rust side.
    assert.deepEqual([...disc("register_product")], [224, 97, 195, 220, 124, 218, 78, 43]);
    assert.deepEqual([...disc("reactivate_product")], [111, 53, 231, 254, 125, 104, 3, 193]);
    assert.deepEqual([...disc("update_product_config")], [148, 82, 249, 211, 243, 20, 93, 174]);
    assert.deepEqual([...disc("deposit_fee")], [11, 51, 105, 140, 198, 229, 7, 77]);
    assert.deepEqual([...disc("request_payout")], [5, 176, 110, 197, 172, 177, 64, 200]);
    assert.deepEqual([...disc("flag_trader_failed")], [60, 230, 114, 103, 27, 235, 37, 129]);
  });

  it("registers a product and inits the ProductRegistry PDA correctly", async () => {
    const fakeSectorProgramId = Keypair.generate().publicKey;
    const [productRegistry] = productRegistryPda(fakeSectorProgramId);
    const challengeSizes: ChallengeSize[] = [{ size: 10_000, cost: 100 }];

    await send(
      registerProductIx(fakeSectorProgramId, 6500, challengeSizes, 5, productRegistry),
      [sl8Admin, rovAdmin]
    );

    const info = await connection.getAccountInfo(productRegistry);
    assert.isNotNull(info);
    const registry = decodeProductRegistry(info!.data);
    assert.isTrue(registry.productProgramId.equals(fakeSectorProgramId));
    assert.equal(registry.feeSplitBps, 6500);
    assert.equal(registry.maxPayoutCount, 5n);
    assert.isTrue(registry.active);
    assert.equal(registry.totalRequestsEmitted, 0n);
    assert.equal(registry.challengeSizes.length, 1);
  });

  it("rejects register_product when one of the two required admin signers is missing", async () => {
    const fakeSectorProgramId = Keypair.generate().publicKey;
    const [productRegistry] = productRegistryPda(fakeSectorProgramId);

    let threw = false;
    try {
      // rovAdmin deliberately omitted from the signers list.
      await send(registerProductIx(fakeSectorProgramId, 6500, [], 5, productRegistry), [sl8Admin]);
    } catch (err) {
      threw = true;
    }
    assert.isTrue(threw, "expected the transaction to fail without rovAdmin's signature");
  });

  it("update_product_config modifies an existing registry entry", async () => {
    const fakeSectorProgramId = Keypair.generate().publicKey;
    const [productRegistry] = productRegistryPda(fakeSectorProgramId);

    await send(registerProductIx(fakeSectorProgramId, 6500, [], 5, productRegistry), [sl8Admin, rovAdmin]);

    const newChallengeSizes: ChallengeSize[] = [{ size: 50_000, cost: 500 }];
    await send(
      updateProductConfigIx(fakeSectorProgramId, newChallengeSizes, 7000, 10, productRegistry),
      [sl8Admin, rovAdmin]
    );

    const info = await connection.getAccountInfo(productRegistry);
    const registry = decodeProductRegistry(info!.data);
    assert.equal(registry.feeSplitBps, 7000);
    assert.equal(registry.maxPayoutCount, 10n);
    assert.equal(registry.challengeSizes.length, 1);
    assert.equal(registry.challengeSizes[0].cost, 500n);
  });

  it("reactivate_product compiles and succeeds against an active registry", async () => {
    // No `pause` instruction exists in Module 1, so there is no way yet to
    // put a registry into a genuinely paused state — this only exercises
    // reactivate_product's auth + PDA lookup against an already-active one.
    const fakeSectorProgramId = Keypair.generate().publicKey;
    const [productRegistry] = productRegistryPda(fakeSectorProgramId);

    await send(registerProductIx(fakeSectorProgramId, 6500, [], 5, productRegistry), [sl8Admin, rovAdmin]);
    await send(reactivateProductIx(fakeSectorProgramId, productRegistry), [sl8Admin, rovAdmin]);

    const info = await connection.getAccountInfo(productRegistry);
    const registry = decodeProductRegistry(info!.data);
    assert.isTrue(registry.active);
  });
});
