# Threat model: core-vault

Companion to [SECURITY-REVIEW.md](SECURITY-REVIEW.md) (what the code does and what was checked) and [DEPLOY-CHECKLIST.md](DEPLOY-CHECKLIST.md) (how to ship it). Finding ids `SR-xx` refer to the findings list in the review. Internal analysis, not an audit.

## 1. Assets

| asset | where | why it matters |
|---|---|---|
| Payout pools | two token accounts (USDC, USDT), authority = the `VaultState` PDA | pays every trader payout and every bond withdrawal |
| Bond obligations | `BondPosition` + `BondCapTracker` + `VaultState.bond_principal_open_total` | a bond holder is owed principal (+ interest at maturity); only **50%** of each principal was ever put in the pool |
| Queued claims | `PayoutClaim` accounts, `open_claims_*` | what the vault owes; paid pro rata by the heartbeat. The total is hard-capped at **$2.5M** (`OPEN_CLAIMS_CEILING`) |
| SL8 revenue stream | SL8 token accounts (owner = the SL8 admin key, by founder decision: SR-18) | fee remainder + half of bond principal + all bond fees. **Whoever holds the SL8 key holds this and, through SR-01, roughly half of every bond** |
| Product registry and trader state | `ProductRegistry`, `TraderState` | decide who may buy, reset, request payouts; carry the request counters reconciliation uses |
| Liveness of the heartbeat | `VaultState` cycle fields | claims are only paid when a cycle runs |
| The two admin keys and the upgrade authority | off-chain | they are the root of every guarantee below |

## 2. Actors

| actor | can do | cannot do |
|---|---|---|
| **Trader** | sign for their own wallet in `deposit_fee` / `deposit_reset` (pay a registered price); own the ATAs that claims are paid to | name a payout amount, a destination, or a price; touch anyone else's record |
| **Bond depositor** | `deposit_bond` / `request_bond_payout` for their own wallet | withdraw another wallet's bond, withdraw twice, withdraw inside the hard lock, exceed $50K / wallet or $600K global |
| **Sector program** (registered) | CPI `deposit_fee`, `deposit_reset`, `record_activity`, `request_payout`, `flag_trader_failed` for **its own product** | act for another product; move tokens directly; choose a trader's destination |
| **Admin pair** (SL8 + Rov, 2-of-2) | register / update / pause / reactivate products; `admin_withdraw_marketing_funds` (75% of a pool per call, to SL8's token account) | change a claim, a bond or a trader; send funds anywhere but SL8's account; act with one key (but see **SL8 key** below) |
| **SL8 key holder** (one key, no second signature) | spend the SL8 token accounts (the revenue); open bonds from wallets they control and take roughly half of each back (SR-01, accepted by the founder; $50K per wallet, $600K in total) | register, pause, update or withdraw alone |
| **Keeper** (anyone) | `begin_heartbeat`, `settle_claims`, `finalize_heartbeat`, `reconcile_product`, `mark_abandoned` | redirect a payment, pay a claim more than once per cycle, pause a healthy product with junk |
| **Attacker** | anything a keeper can; dust any address; create/close/freeze their *own* token accounts; run bonds | forge a program-owned account, a sector signature or an admin signature |
| **Issuer** (Circle / Tether, external) | freeze any token account, including a pool or SL8's account; blacklist | stop the heartbeat (a frozen pool counts as empty since module 4b: SR-03 fixed); n/a otherwise, outside our control (SR-16) |
| **Upgrade-authority holder** (off-chain) | replace the program | n/a — the strongest actor in the system (SR-15) |

## 3. Trust assumptions

1. **Sector programs are trusted via the registry.** The vault authenticates *who* is calling (the `sector_authority` PDA), not whether a payout *amount* is reasonable. A registered sector decides `amount` in `request_payout`; the vault bounds only the count per challenge (`max_payout_count`) and the total of all open claims (the $2.5M ceiling). Reconciliation catches a sector whose books disagree with the vault's, **not** one that over-reports consistently. A per-product limit is deliberately not built because each sector's payout mechanics are not decided yet (SR-02, open; revisit when each product's payout rule exists).
2. **The oracle / trading logic is not in the vault.** Whether a trader really earned a payout is the sector's judgement.
3. **The clock is the validator-supplied `unix_timestamp`.** It is monotone in practice and may differ from wall time by seconds to a minute. Every boundary is tested at +-1 s, and a backwards wobble is handled by saturating arithmetic. Bond lock/maturity and the 7-day/5-day windows are not sensitive to such drift.
4. **USDC = USDT = $1.** Claims are in dollars; either pool pays either claim (larger pool first). A depeg moves value between depositors; there is no oracle.
5. **The stablecoin issuers behave.** USDC and USDT mints have freeze authorities. Since module 4b a frozen pool no longer wedges settlement (SR-03 fixed): it counts as empty, claims are paid from the other pool, and with both frozen they carry over unpaid until a thaw. Deposits into, and admin withdrawals from, a frozen pool still fail. An issuer freeze is still a payout outage, so it is monitored (DEPLOY-CHECKLIST, section 7).
6. **Both admin keys are held by different people on different devices**, and the shared-interfaces crate is pinned (`v0.4.0`) so a wire change cannot slip in. **The SL8 key is also the revenue address and holds the bond-recycling power (SR-01, SR-18, accepted by the founder): its custody must be treated as custody of money.** Bond depositors trust its holder; the bond product is not trustless.
7. **Sector tallies are updated atomically with each accepted `request_payout`** (SR-17).

## 4. Attack trees (top 8)

Notation: `GOAL` <- ways; **[x]** = mitigated in code, **[~]** = mitigated by procedure/assumption, **[!]** = open, listed as a finding.

### T1. Drain the pools through a sector
```
GOAL: take pool funds as a "trader"
 <- compromised / malicious registered sector
      - register is admin-only                                           [x]
      - request_payout with a huge amount for a wallet it controls       [!] SR-02 (open), bounded only by the $2.5M ceiling
          * needs a signed deposit_fee for that wallet (cheap tier)
          * claim settles pro rata: ratio = pool / owed, so a claim far
            above the pool takes (almost) the whole pool and dilutes all
      - request_payout of ~u64::MAX (a sector bug that wraps, or malice)   [x] SR-21 fixed: refused with ClaimsCeilingExceeded; the total of
                                                                           open claims can never pass $2.5M, so the counter cannot overflow
      - request_payout up to the ceiling, then stay unpaid                 [!] SR-02: fills the ~$1.72M headroom (bonds can owe $780K), every new
                                                                           request_payout / request_bond_payout is refused until the pool pays it down
      - reconcile_product pauses only if books DISAGREE                  [~] a consistent over-report passes
 <- an honest sector with a bug that over-requests                       same as above
 <- anyone else calling request_payout                                   [x] needs the sector_authority PDA signature
```
Residual: the vault cannot judge an amount below the ceiling. A per-product limit is deliberately NOT built because the sector payout mechanics are not decided; revisit when each product's payout rule exists. Interim mitigations: 2-of-2 registration, `max_payout_count`, the $2.5M ceiling, reconciliation (blind to a consistent over-report).

### T2. Both admin keys together
```
GOAL: take pool funds
 <- admin_withdraw_marketing_funds x n          75% of the live pool per call, repeatable, no cycle gating   [documented exception]
 <- register_product(malicious sector) then T1
 <- pause / reactivate to freeze or unfreeze products
 <- no way to: edit a claim, a bond or a trader; send to any account but SL8's
```
Residual: total loss of the pool is possible with both keys. Mitigation is custody (two people, two devices) and, later, a multisig + a cooling-off design (needs a decision).

### T3. One key (SL8) recycles bonds to pull pool money  (**SR-01, accepted by the founder**)
```
GOAL: extract pool money with ONE signature (bypasses 2-of-2, the 25% reserve and "no admin key on bond money")
 <- SL8 controls wallets W1..Wn (any keypairs)
      1. each Wi: deposit_bond(P)   pays P + 0.2%.  Pool +P/2.  SL8 token account +P/2 + fee  (SL8 gets half back at once)
      2. after the hard lock (90 or 135 days): request_bond_payout -> claim = P - 0.2%
      3. heartbeat pays the claim from the pool (pro rata)
      net per bond: pool -P/2, SL8 +P/2 (more if held to maturity: +interest)
 limits: $50K per wallet, $600K open globally, 90/135-day lock, claims are diluted pro rata with everyone else
 note: any outside user can do the same economic loop; only SL8 gets half the principal back instantly, so for SL8 the loop is free.
```
**Accepted by the founder** as the same trust class as the admin-withdrawal exception: the holder of the SL8 key, or any wallet they control, can take roughly half of every bond they open, bounded by $50K per wallet and $600K in total, with no second signature. Bond depositors must trust the SL8 key holder; the bond product is not trustless. (Not built, for the record: route SL8's bond share to a locked or 2-of-2 account; refuse the SL8 wallet as a depositor; fund interest from SL8's share; lower the caps.)

### T4. Insolvency / bank run
```
GOAL: make claims unpayable or unfair
 <- bond withdrawals (principal P each) vs a pool that received only P/2 of each bond
      - early withdrawal after the hard lock returns the full principal
      - mass withdrawal -> claims >> pool -> ratio << 1 -> trader payouts diluted [design: equal priority]
 <- admin 75% withdrawals (T2)
 <- trader payouts exceed fee inflow
```
Nothing is created or lost and nobody is paid more than owed (invariants 1, 4, 11), but a claim can wait a long time. This is the economic design, not a bug; the monitoring list in DEPLOY-CHECKLIST tracks the ratio.

### T5. Stuck or starved heartbeat
```
GOAL: stop claims being paid
 <- stop begin_heartbeat:     anyone can call; only the 5-day gap and "no open cycle" block it   [x]
 <- stop settle_claims:
      - bad/closed/frozen/re-owned ATA                                   [x] skipped, counted processed
      - wrong destination address                                        [x] reverts only the submitter
      - front-run a keeper batch (ClaimAlreadySettled)                   [accepted] SR-06
      - grind a wallet so its ATA derivation is expensive                [x] single claim still fits; SR-13
      - issuer freezes a POOL token account                              [x] SR-03 fixed: counts as empty; the other pool pays, or (both frozen)
                                                                         claims carry over unpaid; nothing reverts
      - claims ceiling reached (sector fills the headroom)               [!] SR-02: new requests refused (not settlement); bond exits wait
 <- stop finalize: needs processed == eligible, always reachable (skipped and zero-pay claims count as processed)
 <- keeper outage: the cycle just stays open; new claims wait
 <- issuer freeze of a pool: payouts are reduced or stopped until it thaws  [~] monitored (DEPLOY-CHECKLIST); deposits into it also fail, so traders and bond depositors use the other mint
```

### T6. False or permanent pause
```
GOAL: take a product offline
 <- reconcile_product with junk tally address                            [x] InvalidTally
 <- tamper with the tally account                                        [x] only the sector writes it
 <- sector updates its tally in a later transaction than the vault CPI   [~] SR-17: anyone can reconcile in the gap and pause it
 <- sector over-reports once                                             [~] SR-05 (accepted): counters only grow; repair = upgrade the sector to write the vault's counters into its tally account (no vault change); otherwise register a new product
```

### T7. Bad deploy or upgrade
```
GOAL: control or break the program
 <- deploy the localnet (test-key) build                                 [x] verify-deploy-build.sh; checklist step
 <- deploy with the wrong program id / keypair                           [~] checklist: declare_id! must match the deployed key
 <- keep the upgrade authority on a hot key                              [!] SR-15 (open, pre-mainnet): decide the holder; plan multisig, then revoke
 <- lose an admin key                                                    [~] SR-14 (accepted by the founder): no rotation instruction; the vault PDA seeds contain both keys
```

### T8. Griefing and nuisance
```
 <- cycle-slot burning (empty begin)         delays new claims by <= 1 gap          SR-08
 <- junk bond claims                          costs the attacker locked $50 each      SR-08
 <- tiny claims that pay 0 while ratio < 1    carried at no cost                     SR-08
 <- dusting any PDA                           handled everywhere                     [x]
 <- clock drift                               boundaries tested at +-1 s             [x]
```

## 5. What each compromise can do

| compromised | can | cannot |
|---|---|---|
| **One admin key** (SL8 *or* Rov) | SL8 key: spend SL8's own token accounts (the revenue); run T3 (roughly half of every bond it opens, SR-01). Rov key: nothing of value alone | register, pause, update or withdraw (needs both) |
| **Both admin keys** | T2: drain 75% of a pool per call; register a malicious sector (T1); pause/unpause; reconfigure fee splits and tiers | alter existing claims/bonds/traders; send funds elsewhere than SL8's account; upgrade the program |
| **A malicious / buggy sector** | T1 for its own product only: queue arbitrary claims for wallets that signed a purchase; fill the $2.5M claims headroom so that new requests and bond exits are refused until the pool pays down (SR-02; a single huge request can no longer overflow the counter: SR-21 fixed), fail/abandon its own traders, burn its challenge ids, desync its own tally (self-pause) | touch another product, move tokens directly, choose a destination other than the trader's ATA |
| **A malicious keeper** | begin an empty cycle, front-run batches, choose batch order, skip claims, reconcile at awkward moments | pay a claim wrongly, double-pay, change a destination, finalize early, pause a healthy product |
| **Upgrade-authority holder** | everything (replace the code) | n/a |
| **A trader wallet** | lose its own tokens; have its claim skipped if its ATAs are unusable | affect anyone else |
| **The issuer** | freeze a pool (payouts shrink or pause until a thaw; the heartbeat still completes: SR-03 fixed) or an ATA (that claim is skipped) | wedge the cycle |
