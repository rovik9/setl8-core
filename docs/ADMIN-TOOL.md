# setl8-admin: the admin signing ceremony

`tools/admin` is a command-line tool for the six 2-of-2 admin instructions of the core-vault program: `init_vault`, `register_product`, `update_product_config`, `pause_product`, `reactivate_product` and `admin_withdraw_marketing_funds`. Every one of them needs **both** admin signatures (SL8 and ROV). The two keys are held by different people on different devices and are compiled into the program (they cannot be rotated: SR-14), so the tool works with exactly those keys. It is the signing ceremony: one side **prepares**, each signer **inspects and signs separately**, anyone **submits**.

Companion documents: [DEPLOY-CHECKLIST.md](DEPLOY-CHECKLIST.md) (when to run which step), [SECURITY-REVIEW.md](SECURITY-REVIEW.md) section 9 (what the tool protects against and what it does not), [THREAT-MODEL.md](THREAT-MODEL.md).

> **Never put a real private key in this repository, in a chat, in an environment variable or on a networked machine you do not control.** The tool only ever *reads* a key file, never writes one, never prints key material, and refuses a key file other users can read. Nothing in this repository (and no assistant working on it) needs, reads or handles a real key: the tests use generated throwaway keys plus the PUBLIC test admin keys of the `localnet` build.

## 1. What the tool does

| command | network | what it does |
|---|---|---|
| `plan <instruction> ...` | only to read a nonce / pre-flight (unless `--offline`) | builds the **unsigned** transaction and writes `tx.json` |
| `inspect tx.json` | none (optional `--rpc`) | decodes the file **from its message bytes alone** and prints what it would do |
| `sign tx.json --keypair <file>` | none | runs `inspect`, makes you retype the first 8 characters of the message hash, adds **one** signature |
| `add-signature tx.json --pubkey <p> --signature <base58>` | none | attaches a signature produced elsewhere, after verifying it |
| `send tx.json` | RPC | checks every signature and the cluster, simulates, sends, waits |
| `status` | RPC, read-only | decodes the vault: pools, reserve, claims and ceiling headroom, cycle, bonds, products |
| `nonce-create` / `nonce-advance` | RPC | create the durable-nonce account / invalidate every outstanding signed transaction |

**What travels between machines is one file, `tx.json`.** It holds the serialized message (base64), the signatures collected so far, and some advisory metadata (cluster, description, signer list, message hash). The metadata is for people: `inspect` re-derives all of it from the message bytes and exits non-zero, loudly, if the file disagrees.

### What a transaction contains (the allowlist)

A transaction is signable only if its instructions are exactly

```
[AdvanceNonceAccount]  [SetComputeUnitLimit]  [SetComputeUnitPrice]  Memo  <one admin instruction>
   optional (if a durable      optional           optional         required   to the core-vault program
   nonce is used)
```

and the message is **byte-for-byte** the canonical message the tool itself builds for the decoded contents. Any other program, any extra instruction, a transfer, an unknown or non-admin vault instruction, an account that is not the re-derived PDA or associated token account, a missing signer flag, a different fee payer than the one you declared: `inspect` flags it and `sign` refuses before it asks for anything. The memo is `setl8-admin/1 genesis=<hash>`: it puts the cluster's genesis hash **inside the signed bytes**, so a devnet transaction can never be mistaken for a mainnet one.

## 2. Build and verify the binary

```
scripts/verify-admin-tool-build.sh
```

builds the tool in release mode with **default features** (the REAL admin public keys) and proves the binary contains both real keys and neither public test key. Record the printed `sha256` in your deploy log and build on a second machine to compare. A build with `--features localnet` embeds the PUBLIC TEST keys and is for the test suite only (`setl8-admin version` prints which kind you hold).

The binary is `tools/admin/target/release/setl8-admin`. The tool is its own Cargo workspace with its own `Cargo.lock`: the program's lockfile and `tests-rs`'s lockfile do not change.

## 3. The ceremony, step by step

Roles: the **preparer** (any machine; may be one of the signers) builds the transaction. **Signer SL8** and **signer ROV** each hold their own key file on their own machine. The **submitter** (anyone with network access) sends. The preparer and submitter hold no key; the signers hold only their own.

### Before the first ceremony

1. Build and verify the binary on every machine that will run it (section 2).
2. Each signer's key file is a standard `solana-keygen` JSON keypair file, on that signer's own offline-capable machine, mode `600` (`chmod 600 file`). See section 5 for the custody options and their honest limits.
3. **Create the durable-nonce account once** (the two signatures may be hours or days apart, and an ordinary blockhash lives only ~60-90 seconds). The nonce authority is the SL8 key:
   `setl8-admin nonce-create --cluster <c> --keypair <sl8-key-file>` prints `Nonce account: <NONCE>`. Write it in the deploy log. (The nonce account's own key is generated in memory, used once and discarded; nothing secret is written.)
4. The SL8 admin key's USDC and USDT **associated token accounts must exist** before any fee arrives and before any withdrawal (`status` shows `MISSING` until they do). Any funded wallet can create them for the SL8 key; no SL8 signature is needed.

### For every admin action

| # | who | what | command |
|---|---|---|---|
| 1 | anyone | read the state | `setl8-admin status --cluster <c>` |
| 2 | preparer | build the unsigned transaction | `setl8-admin plan <instruction> ... --cluster <c> --nonce-account <NONCE> --out tx.json` |
| 3 | preparer | send `tx.json` to signer 1 **and** tell the message hash over a second channel (phone call, a different messenger) | the hash is printed by `plan`: `Message SHA-256` |
| 4 | signer | inspect on **your own machine**; check every item of section 4; compare the hash with what the preparer told you | `setl8-admin inspect tx.json --nonce-account <NONCE>` (add `--rpc <url>` on a networked machine for the online checks) |
| 5 | signer | sign; retype the first 8 characters of the hash | `setl8-admin sign tx.json --keypair <my-key-file> --nonce-account <NONCE> --out tx.json` |
| 6 | signer 1 -> signer 2 | pass the file on; signer 2 repeats steps 4 and 5. Signer 2's `inspect` shows signer 1's signature as `present, VALID` | same commands |
| 7 | submitter | send | `setl8-admin send tx.json --cluster <c> --nonce-account <NONCE>` (mainnet: add `--i-understand-this-is-mainnet`, then type `MAINNET`) |
| 8 | anyone | read the state again; compare with what you expected | `setl8-admin status --cluster <c>` |
| 9 | SL8 signer | if the transaction will **not** be sent after all, revoke it | `setl8-admin nonce-advance --cluster <c> --keypair <sl8-key-file> --nonce-account <NONCE>` |

Add `--fee-payer <pubkey>` to steps 2, 4, 5 and 7 only if somebody other than the SL8 key pays the fee (then a third signature is needed). If you use a non-default program id (before deploy, `declare_id!` changes), pass `--program-id <pubkey>` on **every** command on **every** machine.

For a same-session action (both signers at one desk), `--recent-blockhash` replaces `--nonce-account`. It prints a warning: the transaction dies ~60-90 seconds after the blockhash was fetched.

### The commands for each of the six instructions (copy-paste)

Replace `<c>` with `devnet`, `mainnet` or `localnet`, and `<NONCE>` with the nonce account. `$T` is the binary.

| instruction | plan |
|---|---|
| `init_vault` | `$T plan init-vault --cluster <c> --usdc-mint <USDC mint> --usdt-mint <USDT mint> --nonce-account <NONCE> --out tx.json` |
| `register_product` | `$T plan register-product --config product.json --cluster <c> --nonce-account <NONCE> --out tx.json` |
| `update_product_config` | `$T plan update-product-config --config product.json --cluster <c> --nonce-account <NONCE> --out tx.json` |
| `pause_product` | `$T plan pause-product --product <sector program id> --cluster <c> --nonce-account <NONCE> --out tx.json` |
| `reactivate_product` | `$T plan reactivate-product --product <sector program id> --cluster <c> --nonce-account <NONCE> --out tx.json` |
| `admin_withdraw_marketing_funds` | `$T plan admin-withdraw --pool <usdc\|usdt> --amount 1250.50 --cluster <c> --nonce-account <NONCE> --out tx.json` |

`product.json` (validated with the program's own limits: at most 32 tiers, 8 reset phases, `fee_split_bps <= 10000`):

```json
{"product_program_id": "<sector program id>", "fee_split_bps": 6500,
 "challenge_sizes": [{"size": 10000000000, "cost": 100000000}],
 "max_payout_count": 5, "reset_price_bps": [100, 150]}
```

`plan` prints a warning for things the program accepts but that are usually a typo (a free tier, `max_payout_count` 0, a reset price above 100%).

For `admin-withdraw`, `plan` reads the live pool (RPC) and **refuses** an amount above what the 25% reserve leaves (`max(stored floor, ceil(25% of the live balance))`), a frozen pool, a missing or frozen SL8 destination account and a zero amount. `--offline` skips that and says so. `inspect` works without a network and says so; with `--rpc` it re-checks the mint against the vault's own record and the amount against the pool as it is now.

## 4. What each signer must check in `inspect` before signing

`inspect` prints everything below from the message bytes. Read it; do not skim.

1. **No `MISMATCH` banner.** If it appears, stop: the file's metadata does not describe its own bytes.
2. **Cluster**: the name and the genesis hash. `mainnet-beta` or `devnet` as you expect. A mainnet transaction also prints a banner in `sign`.
3. **Program**: it must say *this build's compiled-in program id*. `OVERRIDDEN with --program-id` is acceptable only if you passed that flag yourself.
4. **Vault PDA**: re-derived from the program id and both admin keys of *your* build.
5. **Instruction and arguments in plain units**, and that they are what you were asked to approve:
   * `init_vault`: the two mint addresses (check them against the issuers' official documentation; they can never be changed). Known mainnet USDC/USDT mints are labelled.
   * `register_product`: the sector program id (a registry cannot be corrected or closed), fee split, every tier, payout cap, reset prices.
   * `update_product_config`: the same, replacing the existing configuration.
   * `pause_product` / `reactivate_product`: the sector program id.
   * `admin_withdraw_marketing_funds`: `withdraw 1,250.000000 USDC from the USDC pool` and the **MONEY LEAVES THE PAYOUT POOL** line. The destination is always SL8's own token account.
6. **Accounts of the vault instruction**: every line has a note saying how it was checked (`re-derived`); there must be no `WRONG`.
7. **Fee payer** (the SL8 key by default) and **lifetime** (durable nonce with its authority, or a short-lived recent blockhash).
8. **Signatures**: who has signed, whether each is `VALID`, who is `MISSING`.
9. **Message SHA-256**: compare it with the preparer's, **received over another channel**. It covers every byte you are signing; this is the check that defeats a preparer who also controls the file transport.
10. `RESULT: ... well-formed ...` at the bottom. Anything else says `DO NOT SIGN` and lists why.

`sign` repeats the inspection, refuses to continue on any problem, refuses a key that is not a required signer or has already signed, and only then asks you to retype the first 8 characters of the hash.

## 5. Key custody: the options and their honest limits

The signer must be able to produce an ed25519 signature, by the exact key whose public half is compiled into the program, over the transaction message.

1. **A keypair file on an offline (air-gapped or at least non-browsing) machine** — what the tool supports directly. The file stays on that machine; `inspect` and `sign` need no network. Limits: whoever controls that machine (malware, a stolen laptop, a backup of the file) controls the key; the tool cannot defend against a compromised signer machine (section 7). Use an encrypted disk, mode `600`, no cloud backup of the key file.
2. **A signature produced elsewhere, attached with `add-signature`.** The signer takes the message bytes (`message_b64` in `tx.json`, base64), signs those exact bytes with any ed25519 signer they trust, and returns the base58 signature. `add-signature` verifies it against the message and the public key before storing it. Limit: you need a signer that signs arbitrary message bytes. This path is tested with a software key only.
3. **Hardware wallets are NOT built in this module and are an open decision.** A hardware wallet that can sign a serialized legacy transaction message (possibly only with "blind signing" for a program it does not know) might work through option 2, but this has **not been tried** and its screen would not show the human-readable summary that `inspect` shows. Decide this before mainnet.
4. **The SL8 key currently lives in a phone wallet, which cannot sign these transactions** (it signs through its own app flows, not an arbitrary two-party transaction with a durable nonce). Because the admin public keys are compiled into the program and are part of the vault's address, **the founder must be able to sign with those exact keys, or redeploy.** The ways out, none of which this tool does for you:
   * move the SL8 private key into a form the tool can use (a key file on an offline machine). This exports the key out of the phone wallet's protection; treat the key as exposed to every place it has been, and do it once, deliberately, on a clean machine;
   * or redeploy the program with admin keys that live where signing is possible. That changes the compiled-in constants (a program change, a new program id, a new vault) and is a founder decision outside this tool.

   Note what is at stake (SR-01, SR-18): whoever holds the SL8 key holds the SL8 revenue and, through bonds, roughly half of every bond they open. Choose the custody for that key accordingly.
5. **The ROV key** has the same constraint and the same options.

## 6. Outstanding pre-signed transactions

A transaction signed against a **durable nonce** stays valid until that nonce is advanced. Anybody who holds the fully signed file can submit it, at a moment of their choosing. What that can do is bounded by what you signed (a withdrawal only ever goes to SL8's own token account, and the program re-checks the 25% reserve against the pool as it is **at execution time**, not at signing time), but you should still treat a fully signed file as a loaded instrument.

* Executing **any** transaction that advances the nonce kills every other transaction signed against the same nonce value. Two files signed against the same value cannot both execute.
* **To revoke** a signed-but-unsent transaction: `setl8-admin nonce-advance` (only the nonce authority, the SL8 key, can). It invalidates every outstanding pre-signed transaction built on that nonce value. Do it whenever a ceremony is abandoned, and as housekeeping after a ceremony that left spare signed files.
* Prefer `send`ing promptly; keep partially signed files private (a single signature alone can do nothing, but there is no reason to publish it).
* A recent-blockhash transaction expires on its own after ~60-90 seconds; use it only when both signers are together.

## 7. What is tested, and what the tool does not protect against

The suite is `cargo test --manifest-path tools/admin/Cargo.toml --features localnet` (part of `scripts/test-all.sh`), run against the real program in LiteSVM with signature verification on: 90 tests with the `localnet` keys (12 unit tests, 8 ceremony, 31 tamper, 8 nonce, 24 safety, 5 status, 2 RPC) and 14 tests in the default (real-key) build (12 unit, 2 RPC). Highlights:

| property | tested by |
|---|---|
| all six instructions work end to end through two reloaded files, and one signature never succeeds | `tests/ceremony.rs` |
| the instruction bytes equal what the existing test harness builds | `ceremony::the_plan_bytes_equal_what_the_existing_test_harness_builds` |
| every tamper: flipped byte after a signature, metadata lies, extra instruction, unknown program, wrong program id, wrong vault PDA, non-SL8 destination, unknown discriminator, wrong fee payer, wrong or duplicate signature, garbage files | `tests/tamper.rs` |
| durable nonce works past the blockhash window; `nonce-advance` revokes | `tests/nonce.rs` |
| genesis mismatch refused; mainnet gate cannot be skipped; wrong confirmation does not sign; key-file permissions; no key bytes in any output or file; every `--help` and error path | `tests/safety.rs` |
| `status` matches the real accounts to the base unit, including a frozen pool and a pool at the claims ceiling | `tests/status.rs` |
| the JSON-RPC client, against a local mock server | `tests/rpc_http.rs` |

Mutation testing of the safety checks: 51 deliberate bugs in the tool's checks (allowlist, canonical form, metadata trust, signature handling, cluster binding, mainnet gate, confirmation, pre-flight, nonce, key-file and secrecy rules), all killed by the suite; two survived at first and led to two new tests ([SECURITY-REVIEW.md](SECURITY-REVIEW.md), appendix D).

**Not protected against (be honest about these):**

* **A compromised signer machine.** If malware controls the machine, it controls what `inspect` prints, what you type and what the key signs. The tool gives a signer an independent decode of the bytes; it cannot make a compromised computer trustworthy. Use a clean, dedicated machine for signing.
* **A malicious preparer who also controls what the signer sees** (the same machine, or the signer's screen). The defences are that the signer decodes the bytes locally and compares the message hash over a second channel; if the preparer controls both channels there is nothing left to compare.
* **A wrong-but-well-formed instruction.** The tool shows what a transaction does; it cannot tell you whether you *should* register that sector, withdraw that amount or use those mints. The mint addresses in `init_vault` are not checked against any list except the labels for the two well-known mainnet mints.
* **The honesty of an RPC node** for `status`, `plan`'s pre-flight and `inspect --rpc`. They are conveniences; the program re-checks everything at execution.
* **Key custody and the upgrade authority** (SR-14, SR-15) and the fact that the SL8 key is also the revenue address (SR-18) — see the security review.
* **Hardware wallets** are not supported (section 5).
* The tool was tested against LiteSVM, not a live cluster.

## 8. Exit codes

`0` success; `1` usage or ordinary error; `2` **refused for safety** (the inspection found a problem, a confirmation was wrong, a gate was not passed, a key file is too open); `70` internal error (the message is withheld on purpose, because a panic payload could contain anything).

## 9. Dependencies

The tool is a separate workspace; its lockfile was seeded from `tests-rs/Cargo.lock`, so almost everything is a crate version the repository already used. **Added relative to that lockfile: `ureq 2.12.1`, `webpki-roots 0.26.11`, and a second pin of `setl8-shared-interfaces` (`v0.4.1`).** No build script touches the network, nothing reports telemetry, nothing needs an account, phone number or KYC. The normal dependency tree contains no `solana-metrics`, `reqwest`, `tokio` or `hyper`.

| crate | version | why |
|---|---|---|
| `core-vault` (path) | 0.1.0 | the program's own seeds, PDA derivation, account types, instruction structs, error enum, limits and the admin pubkeys (`localnet` feature mirrors the program) |
| `setl8-shared-interfaces` (git tag) | v0.4.1 | instruction builders for the five admin instructions that have one; v0.4.1 has the correct `admin_withdraw_marketing_funds` builder. The program itself stays on v0.4.0 (it is frozen); cargo treats them as different packages, and `ChallengeSize` is converted field by field |
| `anchor-lang` | =0.32.1 | `Pubkey`, instruction/account serialisation traits (the version the program uses) |
| `anchor-spl` (`token` only) | =0.32.1 | the SPL Token account layout (no Token-2022) |
| `solana-keypair` | =2.2.3 | reading a keypair file and signing |
| `solana-signer` | =2.2.1 | the `Signer` trait |
| `solana-signature` | =2.3.0 | signature type and verification |
| `solana-message` | =2.4.0 | the legacy transaction message |
| `solana-hash` | =2.3.0 | the `Hash` type (blockhash / nonce value) |
| `sha2` | =0.10.9 | the SHA-256 of the message and instruction discriminators |
| `bs58` | =0.5.1 | base58 for signatures and RPC filters |
| `base64` | =0.22.1 | the message in `tx.json` and RPC payloads |
| `bincode` | =1.3.3 | the transaction message wire format |
| `serde`, `serde_json` | =1.0.229, =1.0.151 | `tx.json`, `product.json`, JSON-RPC |
| `zeroize` | =1.9.0 | best-effort wiping of key bytes after use |
| `ureq` (`tls`, `json`) | =2.12.1 | a small blocking HTTPS client for the JSON-RPC calls (the only network code) |

Dev-only (tests): `litesvm =0.7.0`, `solana-account`, `solana-transaction`, `solana-transaction-error` (all already in `tests-rs`), and `setl8-shared-interfaces v0.4.0` under the name `si040` (to prove the plan bytes equal what the existing harness builds).
