# Devnet rehearsal (module 6)

Companion to [DEPLOY-CHECKLIST.md](DEPLOY-CHECKLIST.md), [ADMIN-TOOL.md](ADMIN-TOOL.md) and [SECURITY-REVIEW.md](SECURITY-REVIEW.md). The goal of the module is to run the whole protocol on a real cluster (devnet) with the `localnet`-keys build, driving every admin step through `setl8-admin`, and to write down what differs from LiteSVM.

## 0. Status: read this first

**The run on devnet itself has NOT happened yet.** The public devnet faucet refused every request (`429: You've either reached your airdrop limit today or the airdrop faucet has run dry`, for 5, 2, 1 and 0.1 SOL, over more than an hour of retries with backoff; the limit is per IP and per day). Deploying the 532,600-byte vault needs about 7.5 SOL (rent 3.7 SOL, doubled while the deploy buffer exists) and no account-free faucet gives that. The throwaway keys therefore hold 0 SOL. Everything else is built, tested and committed so the run is **one command** once the keys are funded (section 3). No real key, no mainnet URL and no personal RPC endpoint was used.

What was done instead, and what the results below are:

* The whole rehearsal driver was run against a **local `solana-test-validator`** (Agave 4.0.2): a real bank, real fees, real compute metering, the real token / associated-token / system / memo programs and real durable nonces, but a private cluster. **All 95 checks pass.** The tables below are from that run and are labelled so. They are *not* devnet results; devnet may differ in feature activation (see section 6) and in latency.
* The devnet copy of the vault (throwaway program id, `localnet` feature = PUBLIC test admin keys) was built in a scratch tree; the real tree stayed byte-identical.

## 1. What the rehearsal covers

`tools/devnet-rehearsal` (a Rust driver) plus `tools/devnet-sector` (a deliberately tiny, insecure **mock sector program**: test scaffolding, never for mainnet; it CPIs `deposit_fee` and `request_payout` into the vault, signs as the `sector_authority` PDA, keeps the payout tally the vault's `reconcile_product` reads, and has a `SetTally` switch to make it wrong). Every admin step runs through `setl8-admin`'s own `plan`, `inspect` (with `--rpc`), `sign` (SL8), `inspect`, `sign` (ROV), `send` code, in process, each stage reloading the transaction file from disk, with a durable nonce. The one difference from the binary: the confirmation ("retype the first 8 characters of the hash") is answered by a scripted host from the hash the tool just printed, because the real binary reads `/dev/tty`, which a script cannot feed.

| step | what |
|---|---|
| S00 | genesis check, programs deployed, own USDC-like and USDT-like mints (6 decimals, freeze authority = a throwaway key), all token accounts (**SL8's first, before any fee**), test balances |
| S01 | `nonce-create`; `status` of an empty vault says "not initialised" cleanly |
| S02 | `init_vault` through the ceremony; `status` |
| S03 | `register_product` with **32 tiers and 8 reset phases** (the program's maximum); the sector initialises its tally; `status` |
| S04 | three traders buy a $50 challenge (USDC, USDT, USDT): exact 65% pool / 35% SL8 split and exact trader debit |
| S05 | two $50 bonds (one per mint): exact split (25.000000 to the pool; 25.100000 to SL8 with the 0.2% fee rounded up); `request_bond_payout` refused with `BondLocked` |
| S06 | payout requests: ceiling + 1 base unit refused (`ClaimsCeilingExceeded`), wrong request id refused (`RequestIdMismatch`), three claims queued; `status` shows the claims and the headroom to the $2,500,000 ceiling |
| S07 | `reconcile_product` matches; the mock's wrong tally makes it **auto-pause and still return Ok** (reason 2); repair, reactivate through the ceremony, reconcile matches again |
| S08 | `begin_heartbeat` snapshot; a second `begin` refused (`CycleInProgress`); settlements checked against the program's own planner to the base unit; `finalize_heartbeat` floors; `begin` again refused (`HeartbeatTooEarly`) |
| S09 | `pause_product` through the ceremony **while the cycle is open**; queued claims still settle while paused; `reactivate_product` |
| S10 | the mint's freeze authority freezes the USDC pool **inside the open cycle**: a deposit into it fails in the token program (`AccountFrozen`, Custom 17); `plan` refuses a withdrawal from it and a hand-built one fails with Custom 17; `settle_claims` does **not** revert, pays from the USDT pool only and leaves a carry-over; thaw; the next claim is paid; finalize |
| S11 | `admin_withdraw_marketing_funds`: `plan` pre-flight refuses max + 1 base unit; the program also refuses it when the pre-flight is bypassed (`WithdrawalExceedsReserve`); the exact maximum succeeds through the ceremony and leaves exactly the reserve |
| S12 | a fully signed unsent transaction, then `nonce-advance`: the tool refuses to send it and the cluster rejects the raw bytes (`BlockhashNotFound`) |
| S13 | tool safety: wrong key refused, one signature refused, a byte changed after the first signature refused, a transaction bound to another cluster refused by genesis hash |

## 2. Keys (all throwaway, outside the repo)

```
umask 077; mkdir -p ~/.setl8-devnet && chmod 700 ~/.setl8-devnet; cd ~/.setl8-devnet
for n in program-vault program-sector upgrade-authority usdc-mint usdt-mint freeze-authority \
         trader1 trader2 trader3 bonder1 bonder2 keeper; do
  solana-keygen new --no-bip39-passphrase --silent -o $n.json; done
cp <repo>/tests/fixtures/sl8-admin.json sl8-test.json     # PUBLIC test admin keys (committed on purpose)
cp <repo>/tests/fixtures/rov-admin.json rov-test.json
chmod 600 *.json
```

The `localnet` feature embeds the **public** test admin keys; their private halves are committed in `tests/fixtures/`. **Anyone can therefore sign admin actions on this deployment** (init, register, pause, withdraw to the SL8 test key's token account). That is acceptable **only on devnet** with worthless tokens, and is why the throwaway mints are our own. `.gitignore` keeps these file names and `rehearsal-*` artefacts out of the repo, and `scripts/secret-scan-staged.sh` fails a commit whose staged diff holds a 64-number JSON array or a base58 string that decodes to 64 bytes.

Always pass `--url` and `--keypair` explicitly to `solana`; never rely on its configuration (it may point at your own wallet and an RPC URL with an API key). The script does.

## 3. Repeating it

```
scripts/devnet-rehearsal.sh --cluster devnet build     # scratch devnet copy of the vault + the mock sector + the driver
scripts/devnet-rehearsal.sh --cluster devnet needs     # which keys still need how much SOL (exit 3 if any)
scripts/devnet-rehearsal.sh --cluster devnet fund      # airdrop with backoff (the faucet is rate limited)
scripts/devnet-rehearsal.sh --cluster devnet deploy    # solana program deploy for both programs (skips deployed ones)
scripts/devnet-rehearsal.sh --cluster devnet run       # the rehearsal; results in ~/.setl8-devnet/rehearsal-work-devnet/
scripts/devnet-rehearsal.sh --cluster devnet all
tools/devnet-rehearsal/selftest-local.sh               # the same driver against a local validator (no SOL needed)
```

The script accepts **only** `--cluster devnet`, only an RPC URL that contains `devnet` and not `mainnet`, and only a node whose genesis hash is devnet's (`EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG`). `run` is idempotent where it can be (nonce, mints, token accounts, init, register, purchases and claims are skipped when they exist); the heartbeat section is one-shot per cycle because the 432,000 s gap cannot be waited out, and says so on a second run.

**Funding still needed on devnet** (the faucet gives at most 5 SOL per request):

| key | public address | SOL | why |
|---|---|---:|---|
| upgrade-authority | `HQoL1GC9UJ1n7G9DWcJJbx1zrRbkvG7mLfafuRgVcuaz` | 8 (7.6 is the floor) | rent of both programs, doubled while the deploy buffer exists |
| keeper | `xTXqsPsZW5QHuD5pQRubd2ipPHTLtukohVHxb6Wc641` | 0.5 | pays mints, token accounts, claims, trader states, fees |
| sl8-test (public test key) | `9SWJUtUBp1AyqfmAAFffbRfb4Qnr2u6Jqek8vHZMkFKP` | 0.5 | fee and rent payer of the admin transactions and the nonce |
| bonder1 | `Eq6nTXmJGXtHg1pqmsCFw7paLjGtLJds1pjhvG95uxz8` | 0.05 | rent of its bond position |
| bonder2 | `58qnaF3TArQKV3r6m3axdk2TVvr8z3RGxg6BjYS5AKds` | 0.05 | rent of its bond position |

## 4. Addresses (public, devnet-only; never keys)

| what | address |
|---|---|
| core-vault program (throwaway id, `localnet` keys build) | `BC2ST6twvVajVuBd2djCvGrM1e6y4ahv8X6g9TPesNxu` |
| core-vault `.so` sha256 (devnet build) | `92d875ec1fd4c2f66d6cf39efb1fdf7bd4d7cfe04bf25888c8485c42d3af3163` (rebuilt twice, identical) |
| mock sector program | `FQG72XYUkeSnNWf8FvcNpUz81ZmRjGXCEn4dfAeVksZi` |
| mock sector `.so` sha256 | `608cabb5eccd3e0bd517142f6af84dd70a68af641cbb4de1e960be6e489969ac` |
| USDC-like mint (6 decimals, freeze authority `CJovXuRcuRdR1FRwPC8hU1hbtFyANNDw7GRqXr8oMiQZ`) | `EdbnTVbkZuMFdGbmP4bfqDZEKpeBde8dMzArS9aymgWa` |
| USDT-like mint | `2dusTAehiFpvcS48mZFAZqYFCtXKNyptEYxQjp6pZAmz` |
| vault PDA | `87NNHBpPpoJPCrUnSewsAd5JB5v7oJjumHYd1KkD4qzT` |
| USDC pool / USDT pool | `HDqRaMdLA1aNadUtYXRtHytYDmGv8EdRmom8jDwFpJVD` / `91hzN985H2VTYhdLbHHmtAVU3LspLchtPdweacpMrTBL` |
| product registry of the mock sector | `88qfavMMB8udCLpCAZsr1NW8gvCcmv8vWNi4bYxcV9Pq` |
| SL8 / ROV admin (PUBLIC test keys) | `9SWJUtUBp1AyqfmAAFffbRfb4Qnr2u6Jqek8vHZMkFKP` / `D1EuhXLMWzz9Ypy9gkkwjpzVhEXi4RoDc6NQZv9og1m6` |
| durable nonce account | created by the run (S01) |

## 5. Results: local validator (Agave 4.0.2), 95 checks, 0 failed

Signatures are cut to 20 characters. Compute units are the whole transaction's, as reported by the node; the admin transactions sent by the tool include the nonce advance and the 25,000-CU memo (section 6). Sizes are bytes of the serialised transaction (shown where the driver built the transaction itself).

| step | check | expected | actual | result | signature | CU | fee (lamports) | tx bytes | ms |
|---|---|---|---|---|---|---:|---:|---:|---:|
| S00 | node genesis hash is the expected cluster | 3sz7Xa9rHY8PwfE9BX3uPmSCJF1qZoDhJjoZ2STbiVjE | 3sz7Xa9rHY8PwfE9BX3uPmSCJF1qZoDhJjoZ2STbiVjE | PASS |  |  |  |  |  |
| S00 | core-vault program BC2ST6twvVajVuBd2djCvGrM1e6y4ahv8X6g9TPesNxu is deployed | executable | executable | PASS |  |  |  |  |  |
| S00 | mock sector program FQG72XYUkeSnNWf8FvcNpUz81ZmRjGXCEn4dfAeVksZi is deployed | executable | executable | PASS |  |  |  |  |  |
| S00 | create usdc-mint mint EdbnTVbkZuMFdGbmP4bfqDZEKpeBde8dMzArS9aymgWa (6 decimals, freeze authority = throwaway key) | created | created | PASS | `36hzNo4YzvpfMY6ebfXq` | 364 | 10000 | 422 | 603 |
| S00 | create usdt-mint mint 2dusTAehiFpvcS48mZFAZqYFCtXKNyptEYxQjp6pZAmz (6 decimals, freeze authority = throwaway key) | created | created | PASS | `3FEBbek5ygciZrWbjxQ6` | 364 | 10000 | 422 | 600 |
| S00 | create 6 associated token accounts (SL8 wallet first in the list) | created | created | PASS | `5WhtniparnwmM7bT1wAA` | 94608 | 5000 | 642 | 605 |
| S00 | create 6 associated token accounts (SL8 wallet first in the list) | created | created | PASS | `5PS4xrSnMieQeLF75EUA` | 88608 | 5000 | 642 | 605 |
| S00 | sl8-test has both token accounts | both exist | checked | PASS |  |  |  |  |  |
| S00 | trader1 has both token accounts | both exist | checked | PASS |  |  |  |  |  |
| S00 | trader2 has both token accounts | both exist | checked | PASS |  |  |  |  |  |
| S00 | trader3 has both token accounts | both exist | checked | PASS |  |  |  |  |  |
| S00 | bonder1 has both token accounts | both exist | checked | PASS |  |  |  |  |  |
| S00 | bonder2 has both token accounts | both exist | checked | PASS |  |  |  |  |  |
| S00 | mint test tokens to 6 accounts | minted | minted | PASS | `4A7bRnAagKQt86HvKwtV` | 720 | 10000 | 608 | 601 |
| S00 | mint test tokens to 4 accounts | minted | minted | PASS | `4mbKP7gQXGc5UpqXyJyH` | 480 | 10000 | 514 | 605 |
| S01 | nonce-create (SL8 key is the authority) | created | created | PASS | `cNAN4HpnkezAp3PMGdaq` | 300 | 10000 |  | 2006 |
| S01 | status of an empty vault reports 'not initialised' cleanly | vault does not exist yet | vault does not exist yet | PASS |  |  |  |  |  |
| S02 | init_vault through the ceremony (plan, inspect, sign, sign, send) | vault with our mints, pools, SL8 wallet | vault with our mints, pools, SL8 wallet | PASS | `3KkTj2M3traCNxdpXw1B` | 49254 | 10000 | 745 | 2006 |
| S02 | status after init_vault | mints and both pools listed | mints and both pools listed | PASS |  |  |  |  |  |
| S03 | register_product with 32 tiers and 8 phases through the ceremony | registered, active | registered, active | PASS | `eqFBdKRGn9Qmc74ANQ8b` | 37026 | 10000 | 1095 | 2006 |
| S03 | mock sector initialises its payout tally (0/0) before the first request | created | created | PASS | `652CpQKSgB7zJ4kNUixH` | 5510 | 5000 | 237 | 605 |
| S03 | status lists the product as ACTIVE | ACTIVE | ACTIVE | PASS |  |  |  |  |  |
| S04 | trader1 buys a $50 challenge in usdc-mint: pool gets floor(65%) | 32.500000 | 32.500000 | PASS | `4aR8A5p15DaZNhojuCcq` | 44063 | 10000 | 655 | 602 |
| S04 | trader1: the SL8 wallet gets the remainder | 17.500000 | 17.500000 | PASS |  |  |  |  |  |
| S04 | trader1: the trader pays exactly the cost | 50.000000 | 50.000000 | PASS |  |  |  |  |  |
| S04 | trader2 buys a $50 challenge in usdt-mint: pool gets floor(65%) | 32.500000 | 32.500000 | PASS | `pSAGXNVRU4MfcEx2grvh` | 41069 | 10000 | 655 | 601 |
| S04 | trader2: the SL8 wallet gets the remainder | 17.500000 | 17.500000 | PASS |  |  |  |  |  |
| S04 | trader2: the trader pays exactly the cost | 50.000000 | 50.000000 | PASS |  |  |  |  |  |
| S04 | trader3 buys a $50 challenge in usdt-mint: pool gets floor(65%) | 32.500000 | 32.500000 | PASS | `5nfhK5kv5ycuiW5YBKzx` | 42569 | 10000 | 655 | 601 |
| S04 | trader3: the SL8 wallet gets the remainder | 17.500000 | 17.500000 | PASS |  |  |  |  |  |
| S04 | trader3: the trader pays exactly the cost | 50.000000 | 50.000000 | PASS |  |  |  |  |  |
| S05 | bonder1 bonds $50 in usdc-mint: half the principal goes to the pool | 25.000000 | 25.000000 | PASS | `5BXvGhmuRKiiVoxiAwe9` | 27084 | 10000 | 588 | 601 |
| S05 | bonder1: SL8 gets the other half plus the 0.2% fee (rounded up) | 25.100000 | 25.100000 | PASS |  |  |  |  |  |
| S05 | bonder1: the depositor pays principal + fee | 50.100000 | 50.100000 | PASS |  |  |  |  |  |
| S05 | bonder1: request_bond_payout right away is refused (90-day lock, cannot be waited out on a cluster) | BondLocked = Custom(6039) | BondLocked = Custom(6039) | PASS |  | 12684 |  | 447 |  |
| S05 | bonder2 bonds $50 in usdt-mint: half the principal goes to the pool | 25.000000 | 25.000000 | PASS | `2XggpGLgW3umsJJfAmsc` | 22590 | 10000 | 588 | 605 |
| S05 | bonder2: SL8 gets the other half plus the 0.2% fee (rounded up) | 25.100000 | 25.100000 | PASS |  |  |  |  |  |
| S05 | bonder2: the depositor pays principal + fee | 50.100000 | 50.100000 | PASS |  |  |  |  |  |
| S05 | bonder2: request_bond_payout right away is refused (90-day lock, cannot be waited out on a cluster) | BondLocked = Custom(6039) | BondLocked = Custom(6039) | PASS |  | 11184 |  | 447 |  |
| S05 | status shows the bond principal | $100 of bond principal | $100 of bond principal | PASS |  |  |  |  |  |
| S06 | a payout request of ceiling + 1 base units ($2,500,000.000001) is refused | ClaimsCeilingExceeded = Custom(6043) | ClaimsCeilingExceeded = Custom(6043) | PASS |  | 28593 |  | 459 |  |
| S06 | a payout request with a wrong request id (5, expected 1) is refused | RequestIdMismatch = Custom(6009) | RequestIdMismatch = Custom(6009) | PASS |  | 33077 |  | 459 |  |
| S06 | mock sector queues a 30.000000 claim for trader1 | claim created | claim created | PASS | `cJPfoLiZBmH7qjj7HPSi` | 36441 | 5000 | 459 | 603 |
| S06 | mock sector queues a 70.000000 claim for trader2 | claim created | claim created | PASS | `t9jzQTXyekRUbXV1rJxr` | 39441 | 5000 | 459 | 603 |
| S06 | mock sector queues a 20.000000 claim for trader3 | claim created | claim created | PASS | `3bz2GmbsjVmJZU3d6VzW` | 37941 | 5000 | 459 | 602 |
| S06 | vault counters: open claims | 3 | 3 | PASS |  |  |  |  |  |
| S06 | vault counters: open claims total | 120.000000 | 120.000000 | PASS |  |  |  |  |  |
| S06 | status shows the claims and the headroom to the $2,500,000 ceiling | headroom 2,499,880.000000 | 2,499,880.000000 | PASS |  |  |  |  |  |
| S07 | reconcile_product with a matching tally leaves the product active | true | true | PASS | `GktnPRxKE3HRPigFWn8Z` | 9801 | 5000 | 276 | 600 |
| S07 | a wrong tally makes reconcile_product auto-pause the product AND return Ok | paused, reason 2 (reconciliation deficit), tx succeeded | active=false, reason=2 | PASS | `4AJet63G1uXHhjyrjXJc` | 11033 | 5000 | 276 | 605 |
| S07 | status names the reconciliation pause | RECONCILIATION DEFICIT | RECONCILIATION DEFICIT | PASS |  |  |  |  |  |
| S07 | reactivate_product through the ceremony re-activates the product | true | true | PASS | `4jy7h2dwmqo518Utqf2r` | 33446 | 10000 | 547 | 2001 |
| S07 | after the tally is repaired reconcile matches again | true | true | PASS | `4Na19BJ79ywEAFNJZPW8` | 9801 | 5000 | 276 | 605 |
| S08 | begin_heartbeat snapshots owed / available / eligible | 120.000000 / 147.500000 / 3 | 120.000000 / 147.500000 / 3 | PASS | `38gjj6kCHFP4PYVX48Wn` | 5670 | 5000 | 277 | 605 |
| S08 | a second begin_heartbeat while the cycle is open | CycleInProgress = Custom(6025) | CycleInProgress = Custom(6025) | PASS |  | 6244 |  | 277 |  |
| S08 | ordinary settle: USDC paid to trader1 | 0.000000 | 0.000000 | PASS | `54yEEnHUVrYLZLFazTWa` | 22557 | 5000 | 475 | 605 |
| S08 | ordinary settle: USDT paid to trader1 | 30.000000 | 30.000000 | PASS |  |  |  |  |  |
| S08 | ordinary settle: pools lose exactly what was paid | 0.000000 / 30.000000 | 0.000000 / 30.000000 | PASS |  |  |  |  |  |
| S08 | ordinary settle: remaining owed on trader1's claim | closed | closed | PASS |  |  |  |  |  |
| S10 | the mint's freeze authority freezes the USDC pool token account | true | true | PASS | `5uFv8h5YvXudAyfMyqG8` | 137 | 10000 | 333 | 602 |
| S10 | status shows the frozen pool | frozen: YES | frozen: YES | PASS |  |  |  |  |  |
| S10 | a trader's deposit_fee into the FROZEN USDC pool fails cleanly in the token program | AccountFrozen = Custom(17) | AccountFrozen = Custom(17) | PASS |  | 33570 |  | 655 |  |
| S10 | setl8-admin plan refuses a withdrawal from the frozen pool | refused (exit 2, FROZEN) | exit 2: error: refused: the USDC pool is FROZEN by the issuer; a withdrawal would fail inside the token program | PASS |  |  |  |  |  |
| S10 | admin_withdraw from the frozen pool (hand-built, no pre-flight) fails in the token program | AccountFrozen = Custom(17) | {"InstructionError":[0,{"Custom":17}]} | PASS | `5vJeR2AQ1o974yK9SfZm` | 9534 | 10000 | 449 | 604 |
| S09 | pause_product through the ceremony | paused, reason 1 (planned upgrade) | active=false, reason=1 | PASS | `1QgbXFEqsyHnagWkSTqA` | 33445 | 10000 | 547 | 2006 |
| S10 | settle with USDC frozen: USDC paid to trader2 | 0.000000 | 0.000000 | PASS | `4FDqXi81Zh3YEKYS6jLc` | 22662 | 5000 | 475 | 605 |
| S10 | settle with USDC frozen: USDT paid to trader2 | 60.000000 | 60.000000 | PASS |  |  |  |  |  |
| S10 | settle with USDC frozen: pools lose exactly what was paid | 0.000000 / 60.000000 | 0.000000 / 60.000000 | PASS |  |  |  |  |  |
| S10 | settle with USDC frozen: remaining owed on trader2's claim | 10.000000 | 10.000000 | PASS |  |  |  |  |  |
| S10 | the carry-over remains owed after the frozen-pool settlement | claim still open with a remainder | Some(10000000) | PASS |  |  |  |  |  |
| S10 | the freeze authority thaws the USDC pool | false | false | PASS | `CFEZGJ5nwALWpq4oS63E` | 134 | 10000 | 333 | 601 |
| S10 | settle after the thaw: USDC paid to trader3 | 20.000000 | 20.000000 | PASS | `3CSBW88eWGWY3Am4en7a` | 22556 | 5000 | 475 | 603 |
| S10 | settle after the thaw: USDT paid to trader3 | 0.000000 | 0.000000 | PASS |  |  |  |  |  |
| S10 | settle after the thaw: pools lose exactly what was paid | 20.000000 / 0.000000 | 20.000000 / 0.000000 | PASS |  |  |  |  |  |
| S10 | settle after the thaw: remaining owed on trader3's claim | closed | closed | PASS |  |  |  |  |  |
| S08 | finalize_heartbeat: USDC floor = floor(25% of the real balance) | 9.375000 | 9.375000 | PASS | `5j8MuGKZp7fZ9RmepMid` | 5645 | 5000 | 277 | 601 |
| S08 | finalize_heartbeat: USDT floor = floor(25% of the real balance) | 0.000000 | 0.000000 | PASS |  |  |  |  |  |
| S08 | the cycle is closed | false | false | PASS |  |  |  |  |  |
| S08 | open claims after the cycle: one carry-over claim | 1 | 1 | PASS |  |  |  |  |  |
| S08 | begin_heartbeat again right away (432,000 s gap cannot be waited out) | HeartbeatTooEarly = Custom(6027) | HeartbeatTooEarly = Custom(6027) | PASS |  | 6286 |  | 277 |  |
| S09 | reactivate_product through the ceremony | true | true | PASS | `3aAzXyhbUTeSB8i1CFus` | 33446 | 10000 | 547 | 2006 |
| S11 | USDC pool 37.500000 , floor 9.375000 , reserve 9.375000 , withdrawable 28.125000 | - | - | PASS |  |  |  |  |  |
| S11 | plan pre-flight refuses max + 1 base unit | refused (exit 2) naming the maximum | exit 2: error: refused: the USDC pool holds 37.500000 and must keep a reserve of 9.375000 (25% of the live balance, or the stored floor if higher); at most 28.1 | PASS |  |  |  |  |  |
| S11 | the program refuses max + 1 base unit even when the pre-flight is bypassed | WithdrawalExceedsReserve | {"InstructionError":[0,{"Custom":6042}]} | PASS | `kNtznF2ErZQucYvp5c7z` | 9402 | 10000 | 449 | 604 |
| S11 | admin_withdraw of exactly the maximum: pool down by the amount | 28.125000 | 28.125000 | PASS | `3fwbPVgptQkUyk8QRJGr` | 35999 | 10000 | 656 | 2006 |
| S11 | admin_withdraw: SL8's token account up by the amount | 28.125000 | 28.125000 | PASS |  |  |  |  |  |
| S11 | admin_withdraw: the pool keeps exactly its reserve | 9.375000 | 9.375000 | PASS |  |  |  |  |  |
| S11 | admin_withdraw: marketing_withdrawn_usdc | 28.125000 | 28.125000 | PASS |  |  |  |  |  |
| S12 | nonce-advance (SL8 key, the nonce authority) | confirmed | confirmed | PASS |  |  |  |  |  |
| S12 | setl8-admin send of the previously signed tx after nonce-advance | refused (simulation fails) | exit 2: error: refused: the simulation failed ("BlockhashNotFound"); nothing was sent | PASS |  |  |  |  |  |
| S12 | the same bytes sent raw to the cluster after nonce-advance | rejected | rejected by the cluster: Transaction simulation failed: Blockhash not found "BlockhashNotFound" | PASS |  |  |  |  |  |
| S13 | sign with a key that is not a required signer | refused (exit 2) | exit 2: error: refused: HWiQE3nbL1qGxV2cbRjjk7j4mAcU8H8ijdt2JcgcDosa is not one of this transaction's required signers | PASS |  |  |  |  |  |
| S13 | send with only one of the two signatures | refused (exit 2) | sign exit 0; send exit 2: error: refused: signatures are missing or invalid for: D1EuhXLMWzz9Ypy9gkkwjpzVhEXi4RoDc6NQZv9og1m6 | PASS |  |  |  |  |  |
| S13 | the second signer refuses a message changed after the first signature | refused (exit 2, signature no longer verifies) | exit 2 | PASS |  |  |  |  |  |
| S13 | a fully signed transaction bound to another cluster is refused by genesis hash | refused (exit 2) naming both clusters | exit 2: error: refused: the node at 127.0.0.1:8899 is on unrecognised cluster (not mainnet-beta, not devnet) (genesis 3sz7Xa9rHY8PwfE9BX3uPmSCJF1qZoDhJjoZ2STbiV | PASS |  |  |  |  |  |

Final `status` output of that run:

```
Cluster ........ unrecognised cluster (not mainnet-beta, not devnet) (genesis 3sz7Xa9rHY8PwfE9BX3uPmSCJF1qZoDhJjoZ2STbiVjE)
Program ........ BC2ST6twvVajVuBd2djCvGrM1e6y4ahv8X6g9TPesNxu
Admin keys ..... SL8 9SWJUtUBp1AyqfmAAFffbRfb4Qnr2u6Jqek8vHZMkFKP  ROV D1EuhXLMWzz9Ypy9gkkwjpzVhEXi4RoDc6NQZv9og1m6  (PUBLIC TEST keys compiled into this build)
Vault PDA ...... 87NNHBpPpoJPCrUnSewsAd5JB5v7oJjumHYd1KkD4qzT
SL8 wallet ..... 9SWJUtUBp1AyqfmAAFffbRfb4Qnr2u6Jqek8vHZMkFKP  (the SL8 admin key)

Pools (admin_withdraw_marketing_funds keeps max(stored floor, ceil(25% of live)) in each pool):
  USDC mint EdbnTVbkZuMFdGbmP4bfqDZEKpeBde8dMzArS9aymgWa  pool HDqRaMdLA1aNadUtYXRtHytYDmGv8EdRmom8jDwFpJVD
    balance 9.375000   frozen: no
    stored floor 9.375000   reserve 9.375000   admin_withdraw could take NOW: 0.000000
    withdrawn so far: 28.125000
    SL8 token account FzR7FgGLMXWnxJQm2rK17PUkRi4ny4r8N226CCsiz2d1: balance 70.725000
  USDT mint 2dusTAehiFpvcS48mZFAZqYFCtXKNyptEYxQjp6pZAmz  pool 91hzN985H2VTYhdLbHHmtAVU3LspLchtPdweacpMrTBL
    balance 0.000000   frozen: no
    stored floor 0.000000   reserve 0.000000   admin_withdraw could take NOW: 0.000000
    withdrawn so far: 0.000000
    SL8 token account 6wnPFGgsCBy4MjWzbG3ajvFwSxhuCAQBFicAXqzZjAxF: balance 60.100000

Open claims ...... 1 claims owing 10.000000
Claims ceiling ... 2,500,000.000000   headroom 2,499,990.000000 (0% used)

Heartbeat ........ cycle 1   idle   started 2026-10-09 05:40:36 UTC
  snapshot owed 120.000000  available 147.500000  eligible 3  processed 3

Bonds ............ principal open 100.000000 of 600,000.000000 cap; withdrawal fees retained 0.000000

Registered products (1):
  FQG72XYUkeSnNWf8FvcNpUz81ZmRjGXCEn4dfAeVksZi  registry 88qfavMMB8udCLpCAZsr1NW8gvCcmv8vWNi4bYxcV9Pq
    ACTIVE
    fee split 6500 bps, 32 tiers, max payouts 10, requests 3 / 120.000000
```

## 6. What differs between a real validator and LiteSVM

Measured on the local validator and, where stated, confirmed in LiteSVM with identical state.

1. **The token program is far cheaper on a real validator.** One `transfer_checked` costs **105 CU** on the validator (log line `Program Tokenkeg... consumed 105 of ...`, seen in the `deposit_bond` transaction) and **6,147 CU** in LiteSVM (same line from a LiteSVM `admin_withdraw_marketing_funds`; its vault program line is 17,027 against 10,921 on the validator, a difference of 6,106). The validator's token program is evidently the CU-efficient implementation; LiteSVM runs a bundled older one. Every documented CU figure of a token-moving instruction therefore **overstates** the real cost by about 6k per token CPI: `deposit_bond` 36,371 documented vs 27,084 real, `admin_withdraw_marketing_funds` 16,953 vs 10,921, `init_vault` 33,834 vs 24,176, `deposit_fee` 35,188 vs 29,633-32,627 (the rest is PDA bump-search variance). The documented figures stay safe upper bounds. **Not yet confirmed on devnet itself** (it depends on the features activated there).
2. **Compute units depend on state, not only on the instruction.** `pause_product` / `reactivate_product` measure 8,367 / 8,368 CU here against the documented 4,716 / 4,718, because the registry has 32 tiers and 8 phases (it is deserialised and re-serialised). The identical 32-tier case in LiteSVM gives **8,367**: LiteSVM and the validator agree to the unit when the state is equal. The documented pause figure is the small-registry case; **8.4k is the worst case**. `request_payout` (24,811 vs 18,213) shows the same effect plus the random PDA bump searches (about 1,500 CU per extra bump tried), and goes through the mock sector's CPI.
3. **`begin_heartbeat` (5,670) and `finalize_heartbeat` (5,645) are identical to the documented figures, to the unit**, and `settle_claims` for one claim is 22,557 against 24,129 documented (token CPIs again).
4. **The tool's genesis memo costs 24,928 CU** (SPL Memo v3, the same in LiteSVM and on the validator). A whole admin transaction through the tool is about 29-49k CU. That is well inside the default 200,000 CU per instruction: **no ComputeBudget instruction is needed anywhere**, and none is documented as needed. The largest whole transaction seen was 49,254 CU (`init_vault` through the tool). The 6-claim `settle_claims` batch (documented 112,845 CU, 400,000 limit advised) was not exercised on the validator: the rehearsal settles single claims on purpose to test the frozen-pool path claim by claim.
5. **Reads need `confirmed` commitment.** Checking that a just-deployed program is executable at the default (`finalized`) commitment says "not deployed" for ~13 s on a fresh validator; the driver reads at `confirmed`.
6. **Preflight failures carry the program error and logs** in the RPC error (`-32002` with `data.err` and `data.logs`), which is how the driver reports exact errors; a transaction sent with `skipPreflight` lands as a failed transaction and still costs the fee.
7. **A stale durable-nonce transaction is rejected as `BlockhashNotFound`** by the cluster's simulation (and by the node on a raw send), the same text as in LiteSVM. Executing any nonce transaction advanced the nonce as documented.

## 7. Compute units: documented vs measured

"Measured" is the vault program's own consumption from the transaction logs (the whole-transaction figure also contains the mock sector, the memo and the nonce advance).

| instruction | documented CU (LiteSVM) | measured on the cluster (vault program only) | whole-tx CU | difference |
|---|---:|---:|---:|---|
| init_vault | 33834 | 24176 | 49254 | -28.5% **>10%** |
| register_product (32 tiers, 8 phases) | 12002 | 11948 | 37026 | -0.4% |
| deposit_fee (usdc-mint) | 35188 | 32627 | 44063 | -7.3% |
| deposit_fee (usdt-mint) | 35188 | 29633 | 41069 | -15.8% **>10%** |
| deposit_bond | 36371 | 27084 | 27084 | -25.5% **>10%** |
| request_payout | 18213 | 24811 | 36441 | +36.2% **>10%** |
| reconcile_product (match) | 9149 | 9801 | 9801 | +7.1% |
| reconcile_product (mismatch, pauses) | 10293 | 11033 | 11033 | +7.2% |
| reactivate_product | 4718 | 8368 | 33446 | +77.4% **>10%** |
| begin_heartbeat | 5670 | 5670 | 5670 | +0.0% |
| settle_claims (1 claim) | 24129 | 22557 | 22557 | -6.5% |
| pause_product | 4716 | 8367 | 33445 | +77.4% **>10%** |
| finalize_heartbeat | 5645 | 5645 | 5645 | +0.0% |
| admin_withdraw_marketing_funds | 16953 | 10921 | 35999 | -35.6% **>10%** |

Rows marked `>10%` are explained in section 6 (items 1 and 2); none exceeds the documented figure except `request_payout` and the pause/reactivate worst case (state size).

## 8. Operational snags

* **Faucet**: the public devnet faucet is limited per IP per day and can be empty; it was the blocker for the devnet run. Plan for ~9 SOL and fund early. There is no account-free alternative.
* **Deploy rent**: `solana program deploy` allocates `--max-len` bytes (the program's size by default is doubled: ~7.4 SOL, permanently). The script passes `--max-len` equal to the program size (3.7 SOL, and the deploy buffer needs the same again until the deploy ends). A larger future version cannot be deployed over a program with a tight `--max-len` without `solana program extend`.
* **RPC rate limits**: the public endpoints answer HTTP 429; the driver retries with backoff.
* **Token accounts before fees**: the SL8 wallet's token accounts must exist before the first fee or bond (the driver creates them first, as the checklist says); `status` prints `MISSING` otherwise.
* **Nonce timing**: planning against a nonce straight after `nonce-create` worked on the validator (slots advance); `plan` reads the live nonce value every time, so there is nothing to wait for.
* **`solana` configuration**: always pass `--url` and `--keypair`.

## 9. What could not be tested (and why)

* **The real devnet run** (faucet, section 0).
* **Bond maturity and withdrawal**: the 90-day hard lock (and 180/270-day terms) cannot be waited out; only the refusal (`BondLocked`) is tested.
* **A second heartbeat cycle**: the 432,000 s (5-day) gap cannot be waited out; only `HeartbeatTooEarly` is tested, and the carry-over claim stays owed.
* **The real issuers' freeze behaviour**: USDC and USDT freezes are done by Circle and Tether on their own mints; the rehearsal freezes a pool with its own mint's freeze authority, which exercises the same token-program `Frozen` state, not the issuers' policies.
* **Mainnet gates and mainnet genesis**: deliberately never touched. The cross-cluster check uses a made-up genesis hash.
* **Hardware wallets**, **the 6-claim settlement batch** and **the upgrade-authority handover** (multisig, revoke) on a real cluster.
* **Other devnet feature sets** (e.g. whether the efficient token program is active there).

## 10. Findings

**No new security finding** (no new SR number): nothing in the program or the tool behaved differently on a real validator in a way that changes a guarantee. Every check of the protocol passed on the real runtime, including the frozen-pool settlement inside an open cycle, the reconciliation auto-pause that returns Ok, the claims-ceiling and request-id refusals, the exact-boundary withdrawal, and nonce revocation.

Documentation findings (recorded here, with the CU table of [SECURITY-REVIEW.md](SECURITY-REVIEW.md) section 4 annotated): the token-CPI cost difference (6.1), the state-size dependence of CU (6.2), and the memo's CU cost (6.4). The devnet run remains to be done.
