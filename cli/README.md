# ssr-cli

Admin and demo surface for the SSR compliance registry and DvP wrapper.
The CLI is a thin client over `ssr-compliance` and `ssr-dvp-wrapper`;
every command maps 1-to-1 to an on-chain instruction (or a pure
derivation that requires no RPC).

## Install

```sh
cargo build --release -p ssr-cli
# binary at ./target/release/ssr-cli
```

## Configuration

The CLI loads configuration in this order (first match wins):

1. CLI flag (`--rpc-url`, `--keypair`, `--compliance-program`, `--wrapper-program`, `--vault-program`, `--repo-program`, `--lending-program`)
2. Environment variable (`SSR_RPC_URL`, `SSR_KEYPAIR`, `SSR_COMPLIANCE_PROGRAM`, `SSR_WRAPPER_PROGRAM`, `SSR_VAULT_PROGRAM`, `SSR_REPO_PROGRAM`, `SSR_LENDING_PROGRAM`)
3. `~/.config/solana/cli/config.yml` (RPC URL + keypair only)
4. Built-in default (`http://127.0.0.1:8899` for RPC; no default for keypair / programs)

For a live demo, set the envs once:

```sh
export SSR_RPC_URL=http://127.0.0.1:8899          # or your devnet
export SSR_KEYPAIR=~/keys/admin.json
export SSR_COMPLIANCE_PROGRAM=<deployed compliance program ID>
export SSR_WRAPPER_PROGRAM=<deployed wrapper program ID>
export SSR_VAULT_PROGRAM=<deployed vault program ID>
export SSR_REPO_PROGRAM=<deployed repo program ID>
export SSR_LENDING_PROGRAM=<deployed lending program ID>
```

Each program ID is resolved lazily — only when a command actually
needs it. `ssr-cli vault deposit` does not require
`SSR_WRAPPER_PROGRAM`; `ssr-cli compliance verify` does not require
the wrapper / vault / repo / lending IDs at all. So for demos that
exercise only part of the surface (lending-only, compliance-only),
only export the program IDs you'll touch.

The keypair is loaded the same way: any read-only command (`*-cli
... state`, `... status`, `compliance show-registry`, `dvp
authority-address`, `derive ...`) runs without `SSR_KEYPAIR` being
set. Only signing paths (init / register / verify / deposit / open /
repay / liquidate / …) require it. This is what lets an observer
inspect the state of a live demo from a fresh shell without first
configuring an admin key.

## Command surface

```
ssr-cli compliance init-registry
ssr-cli compliance register   --participant <pubkey> [--jurisdiction JP]
ssr-cli compliance verify     --participant <pubkey>
ssr-cli compliance suspend    --participant <pubkey>
ssr-cli compliance block      --participant <pubkey>
ssr-cli compliance status     --participant <pubkey>           # read-only
ssr-cli compliance show-registry                                # read-only
ssr-cli compliance rotate-operator --role onboard|status \
                                   --new-pubkey <pubkey>
ssr-cli compliance init-risk-params                             # super-admin signs
ssr-cli compliance set-haircut --class <name> --bps <n>         # super-admin signs
ssr-cli compliance show-risk-params                             # read-only
ssr-cli compliance set-max-staleness --slots <n>                # super-admin signs
ssr-cli compliance register-price-feed --mint <pk> \
                                       --price-micro-usd <n> \
                                       --mint-decimals <d>      # super-admin signs
ssr-cli compliance update-price        --mint <pk> --price-micro-usd <n>
                                                                # oracle_operator signs
ssr-cli compliance show-price-feed     --mint <pk>              # read-only
ssr-cli compliance bind-price-feed-to-pyth --mint <pk> \
                                           --pyth-source <pk>   # super-admin signs
ssr-cli compliance update-price-from-pyth  --mint <pk>          # oracle_operator signs
ssr-cli compliance init-pyth-config        --pyth-program-id <pk>
                                                                # super-admin signs
ssr-cli compliance set-pyth-program-id     --pyth-program-id <pk>
                                                                # super-admin signs
ssr-cli compliance show-pyth-config                             # read-only

ssr-cli dvp authority-address                                   # no RPC
ssr-cli dvp settle    --swap-dvp <pubkey> [--leg-a-extras-count N]

ssr-cli vault init      --mint <pubkey> [--asset-class equity|stablecoin|...]
ssr-cli vault deposit   --mint <pubkey> --amount <n>            # global --keypair = depositor
ssr-cli vault withdraw  --mint <pubkey> --amount <n>
ssr-cli vault state     --mint <pubkey>                          # read-only
ssr-cli vault position  --mint <pubkey> [--depositor <pubkey>]   # read-only

ssr-cli repo open  --borrower-keypair <path> --lender-keypair <path> \
                   --collateral-mint <pubkey> --cash-mint <pubkey> \
                   --collateral-amount <n>   --cash-amount <n> \
                   --expiry-slot <n>         --nonce <n>
ssr-cli repo close --lender <pubkey> --collateral-mint <pubkey> \
                   --cash-mint <pubkey> --nonce <n>             # global --keypair = borrower
ssr-cli repo state --borrower <pk> --lender <pk> \
                   --collateral-mint <pk> --cash-mint <pk> \
                   --nonce <n>                                  # read-only

ssr-cli lending open  --borrower-keypair <path> --lender-keypair <path> \
                      --collateral-mint <pk> --cash-mint <pk> \
                      --collateral-amount <n>   --principal-amount <n> \
                      --maturity-slot <n>       --nonce <n> \
                      [--interest-bps-per-year <bps>]
ssr-cli lending repay --lender <pk> --collateral-mint <pk> \
                      --cash-mint <pk> --nonce <n>              # global --keypair = borrower
ssr-cli lending state --borrower <pk> --lender <pk> \
                      --collateral-mint <pk> --cash-mint <pk> \
                      --nonce <n>                               # read-only

ssr-cli margin show   --user <pk> --mint <pk> [--mint <pk> ...] # read-only

ssr-cli derive record    --participant <pubkey>                 # no RPC
ssr-cli derive registry                                         # no RPC
ssr-cli derive vault     --mint <pubkey>                        # no RPC
ssr-cli derive position  --mint <pubkey> --depositor <pubkey>   # no RPC
ssr-cli derive repo      --borrower <pk> --lender <pk> \
                         --collateral-mint <pk> --cash-mint <pk> \
                         --nonce <u64>                          # no RPC
ssr-cli derive loan      --borrower <pk> --lender <pk> \
                         --collateral-mint <pk> --cash-mint <pk> \
                         --nonce <u64>                          # no RPC
ssr-cli derive ata       --owner <pubkey> --mint <pubkey>       # no RPC
ssr-cli derive swap-dvp  --settlement-authority <pk> --user-a <pk> \
                         --user-b <pk> --mint-a <pk> --mint-b <pk> \
                         --nonce <u64>                          # no RPC
```

## Demo dramaturgy (recommended order)

The script below walks through the entire Phase 1 scope end-to-end on
localnet — what a live demo attendee would see.

### 0. Local validator + program deploy

```sh
solana-test-validator --reset --quiet &
sleep 2

# Build + deploy programs.
cargo build-sbf --manifest-path programs/ssr-compliance/Cargo.toml
cargo build-sbf --manifest-path programs/ssr-dvp-wrapper/Cargo.toml

COMPLIANCE_PROGRAM=$(solana program deploy target/deploy/ssr_compliance.so | awk '/Program Id/ {print $3}')
WRAPPER_PROGRAM=$(solana program deploy target/deploy/ssr_dvp_wrapper.so   | awk '/Program Id/ {print $3}')

# The SPC dvp-swap-program program ID is fixed; deploy at the upgrade
# authority you control so the test loader honors the canonical ID.
solana program deploy tests/fixtures/dvp_swap_program.so \
  --program-id <path-to-keypair-matching-DzG1qJupt6Khm8s8jB3p93NkhPoiAg2M7vkEhkS15CtC>

export SSR_COMPLIANCE_PROGRAM=$COMPLIANCE_PROGRAM
export SSR_WRAPPER_PROGRAM=$WRAPPER_PROGRAM
export SSR_KEYPAIR=~/.config/solana/id.json
```

### 1. Compliance bootstrap (1 transaction)

```sh
ssr-cli compliance init-registry
# ✓ tx confirmed: 5VbRb...
# registry initialized
#   super_admin       = 4Tn6...        ← signer
#   onboard_operator  = 4Tn6...        ← defaults to super_admin
#   status_operator   = 4Tn6...        ← defaults to super_admin
```

(Optionally: `ssr-cli compliance rotate-operator --role onboard --new-pubkey <ops_kp>`)

### 2. Onboard two participants and verify both

```sh
USER_A=$(solana-keygen new -o /tmp/user_a.json --no-bip39-passphrase --silent | awk '/pubkey/ {print $2}')
USER_B=$(solana-keygen new -o /tmp/user_b.json --no-bip39-passphrase --silent | awk '/pubkey/ {print $2}')

ssr-cli compliance register --participant $USER_A --jurisdiction JP
ssr-cli compliance register --participant $USER_B --jurisdiction JP
ssr-cli compliance verify   --participant $USER_A
ssr-cli compliance verify   --participant $USER_B

ssr-cli compliance status --participant $USER_A
# participant 7H3...
#   status          = VERIFIED (u8 2)
#   jurisdiction    = JP
#   flags           = 0b00000000
#   updated_at_slot = 1247
#   bump            = 254
#   transfer gate   = ✓ verified — transfers allowed
```

### 3. Token-2022 setup (cash + asset mints, no TransferHook)

```sh
spl-token --program-id TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb create-token --decimals 6
spl-token --program-id TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb create-token --decimals 6
# (capture as MINT_ASSET, MINT_CASH)

# Mint to user_a (asset) and user_b (cash) via spl-token CLI as usual.
```

### 4. Create the SPC DvP, wrapper PDA as settlement_authority

```sh
WRAPPER_AUTH=$(ssr-cli dvp authority-address)
echo "wrapper settlement authority: $WRAPPER_AUTH"

# Compute the SwapDvp PDA (no RPC required):
SWAP_DVP=$(ssr-cli derive swap-dvp \
  --settlement-authority $WRAPPER_AUTH \
  --user-a $USER_A --user-b $USER_B \
  --mint-a $MINT_ASSET --mint-b $MINT_CASH \
  --nonce 1)

# Hand-roll the SPC CreateDvp call (the wrapper does not gate Create —
# CreateDvp is permissionless per SPC's spec). Use any SPC client crate
# or the IDL in /Users/hiroyusai/src/spc-reference/dvp-swap-program/idl/.
# After CreateDvp succeeds, each side funds their leg via raw SPL Transfer
# to the escrow ATA (also derivable via the canonical ATA formula).
```

### 5. Compliance-gated atomic settlement (1 transaction)

```sh
ssr-cli dvp settle --swap-dvp $SWAP_DVP
# ✓ tx confirmed: 3Vb...
# - The wrapper read user_a + user_b out of the SwapDvp account.
# - It verified both AccountRecord PDAs (owner = compliance program,
#   participant matches, status VERIFIED).
# - It signed the SettleDvp CPI as the wrapper PDA.
# - SPC SettleDvp atomically delivered asset → user_b and cash → user_a,
#   then closed the SwapDvp PDA + the two escrow ATAs (rent went to the
#   wrapper PDA).
```

### 6. Negative path — suspended counterparty rejects (demonstrates the gate)

```sh
ssr-cli compliance suspend --participant $USER_B

ssr-cli dvp settle --swap-dvp <new SwapDvp PDA for a fresh trade>
# Error: tx failed: Custom(0x2013)   ← COMPLIANCE_SUSPENDED
#                                      (distinct from a hook-side or
#                                       layout-side reject; operators
#                                       see "suspended" specifically)
```

`Reclaim` / `Reject` (SPC primitives, not wrapped) recover the funded
legs — funds are never trapped.

## Phase 2 / 3 demo extension (vault + repo + lending)

The Phase 0/1 demo above ends with atomic DvP settlement. The longer
demo continues into the **collateral vault** + **bilateral lock** path
that supports ongoing positions and time-bound encumbrance — the
foundation for cross-asset repo / lending / margin. Steps 7-10 cover
vault + repo; steps 11-13 add the collateralized term loan primitive.

### 7. Vault setup (admin)

```sh
ssr-cli vault init --mint $MINT_ASSET --asset-class equity
ssr-cli vault init --mint $MINT_CASH  --asset-class stablecoin
# Note: separately, also create each vault PDA's canonical Token-2022 ATA
# (deposits land there). The `derive vault` + `derive ata` commands give
# you the right addresses; `spl-token create-account` on the vault as
# owner creates the ATA.
VAULT_ASSET=$(ssr-cli derive vault --mint $MINT_ASSET)
VAULT_ASSET_ATA=$(ssr-cli derive ata --owner $VAULT_ASSET --mint $MINT_ASSET)
spl-token --program-id TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb \
  create-account $MINT_ASSET $VAULT_ASSET --owner $VAULT_ASSET --fee-payer ~/keys/admin.json
# (repeat for VAULT_CASH)
```

### 8. Depositors fund their positions

```sh
# Switch SSR_KEYPAIR to the borrower's keypair (or pass via --keypair):
SSR_KEYPAIR=~/keys/borrower.json ssr-cli vault deposit \
  --mint $MINT_ASSET --amount 500000

SSR_KEYPAIR=~/keys/borrower.json ssr-cli vault position --mint $MINT_ASSET
# position @ ...
#   amount_deposited   = 500000
#   locked_amount      = 0
#   available          = 500000
#   lock_authority     = (none)

SSR_KEYPAIR=~/keys/lender.json ssr-cli vault deposit \
  --mint $MINT_CASH --amount 400000
```

### 9. Open a repo (both parties sign)

```sh
NONCE=1
EXPIRY=$(($(solana slot --commitment confirmed) + 500_000))

ssr-cli repo open \
  --borrower-keypair ~/keys/borrower.json \
  --lender-keypair   ~/keys/lender.json \
  --collateral-mint  $MINT_ASSET \
  --cash-mint        $MINT_CASH \
  --collateral-amount 300000 \
  --cash-amount       200000 \
  --expiry-slot       $EXPIRY \
  --nonce             $NONCE
# repo @ ...
#   status            = OPEN
#   collateral_amount = 300000
#   cash_amount       = 200000
```

Re-inspect either position to see the lock:

```sh
SSR_KEYPAIR=~/keys/borrower.json ssr-cli vault position --mint $MINT_ASSET
# position @ ...
#   amount_deposited   = 500000
#   locked_amount      = 300000
#   available          = 200000
#   lock_authority     = <Repo PDA>    ← the visible consulting differentiator
```

### 10. Close the repo (borrower signs)

```sh
SSR_KEYPAIR=~/keys/borrower.json ssr-cli repo close \
  --lender           <lender_pubkey> \
  --collateral-mint  $MINT_ASSET \
  --cash-mint        $MINT_CASH \
  --nonce            $NONCE
# repo @ ...
#   status            = CLOSED

SSR_KEYPAIR=~/keys/borrower.json ssr-cli vault position --mint $MINT_ASSET
# position @ ...
#   locked_amount      = 0
#   lock_authority     = (none)
```

### 11. Open a collateralized term loan (both parties sign)

Lending mirrors repo's encumbrance model (Phase 3 minimum): both sides
get locked, the borrower repays before `maturity_slot` to unlock.
`interest_bps_per_year` is recorded but not enforced on-chain in Phase 3
— off-chain settlement applies the computed interest at repay time
(Phase 3b will enforce it on-chain alongside `liquidate_loan`).

```sh
LOAN_NONCE=1
MATURITY=$(($(solana slot --commitment confirmed) + 500_000))

ssr-cli lending open \
  --borrower-keypair      ~/keys/borrower.json \
  --lender-keypair        ~/keys/lender.json \
  --collateral-mint       $MINT_ASSET \
  --cash-mint             $MINT_CASH \
  --collateral-amount     300000 \
  --principal-amount      200000 \
  --maturity-slot         $MATURITY \
  --nonce                 $LOAN_NONCE \
  --interest-bps-per-year 500
# loan @ ...
#   status                = OPEN
#   collateral_amount     = 300000
#   principal_amount      = 200000
#   interest_bps_per_year = 500 (5.00% / yr)
#   maturity_slot         = ...
```

Re-inspect the borrower's collateral position to see the loan-side lock:

```sh
SSR_KEYPAIR=~/keys/borrower.json ssr-cli vault position --mint $MINT_ASSET
# position @ ...
#   locked_amount      = 300000
#   lock_authority     = <Loan PDA>    ← lending program's PDA, distinct from Repo PDA
```

### 12. Repay the loan (borrower signs)

```sh
SSR_KEYPAIR=~/keys/borrower.json ssr-cli lending repay \
  --lender           <lender_pubkey> \
  --collateral-mint  $MINT_ASSET \
  --cash-mint        $MINT_CASH \
  --nonce            $LOAN_NONCE
# loan @ ...
#   status                = REPAID
```

### 13. Negative paths — suspended at open, past maturity at repay

```sh
# Suspended party at open:
ssr-cli compliance suspend --participant $BORROWER
ssr-cli lending open --borrower-keypair ~/keys/borrower.json ... --nonce 2
# Error: tx failed: Custom(0x5013)   ← lending program's COMPLIANCE_SUSPENDED

# Past maturity at repay (after waiting / warping past maturity_slot):
ssr-cli lending repay --lender <pk> --collateral-mint $MINT_ASSET \
                      --cash-mint $MINT_CASH --nonce $LOAN_NONCE
# Error: tx failed: Custom(0x5022)   ← MATURED (route to Phase 3b liquidate path)
```

### Phase 4 v1a: cross-margin view (collateral + loan-side netting)

After the borrower's vault deposit (step 8) and any open repos / loans
that lock collateral, an observer can summarize the borrower's margin
in one read-only call. Mints must be passed explicitly:

```sh
ssr-cli margin show --user $BORROWER \
                    --mint $MINT_ASSET \
                    --mint $MINT_CASH
# margin @ <borrower>
#
# (risk_params @ <PDA>, last_modified_slot N)   ← v1c: live haircut table
#
#   mint <MINT_ASSET> (EQUITY, haircut 3000 bps)
#     deposited            = 500000
#     locked               = 300000   ← collateral pledged to open loan/repo
#     available            = 200000
#     haircut-adjusted     = 140000 (free) / 210000 (encumbered) / 350000 (gross)
#
#   mint <MINT_CASH> (STABLECOIN, haircut 0 bps)
#     deposited            = 400000
#     locked               = 0
#     available            = 400000
#     haircut-adjusted     = 400000 (free) / 0 (encumbered) / 400000 (gross)
#
# open loans (borrowed → liabilities):
#   <Loan PDA>  principal=200000 + interest=1250 = 201250  cash_mint=<MINT_CASH>  → liability
#
# totals across 2 positions (haircut-adjusted):
#   gross collateral     = 750000
#   encumbered           = 210000
#   available collateral = 540000
# liabilities (cash mints the user owes back):
#   <MINT_CASH>: 201250
# net margin per cash mint (no FX across mints in v1a):
#   vs <MINT_CASH>: 540000 − 201250 = 338750
```

How v1a derives liabilities: two `getProgramAccounts` memcmp filters
per program (`borrower==user`, then `lender==user`) on `ssr-lending`
and `ssr-repo`, status-filtered to `OPEN`. Loan liability is
`principal + accrued simple interest`, where the interest formula
mirrors `ssr-lending::compute_simple_interest` exactly (`principal ×
elapsed_slots × bps_per_year / (SLOTS_PER_YEAR × 10_000)`) — one
`getSlot` projects it at the same slot `repay_loan` would see. Repo
liability is `cash_amount` (the off-chain repayment leg). All
liabilities are summed per cash mint and subtracted from `available
collateral`.

Notes and limits:

- **No FX**: liabilities are reported per cash mint; net margin is too.
  An oracle-backed cross-currency netting is reserved for v1b.
- **Optional dependencies**: the lending / repo program IDs are looked
  up lazily — if one isn't configured the view prints `(skipping
  loans — missing ...)` and still renders the rest. Use this for
  lending-only or repo-only demos.
- **Cash mint coverage warning**: if a loan or repo references a cash
  mint that wasn't passed via `--mint`, the user's holdings in that
  mint are excluded from `available collateral` and the output flags
  the omission. Add the missing mint to get a complete picture.
- **Repo cash flow is off-chain in Phase 3 minimum.** The repo cash
  leg is recorded as a liability for the borrower because the borrower
  *will* owe it back at close, even though the cash transfer itself
  isn't settled by `ssr-repo`. Operators who run repos with cash
  settled outside the wrapper should read this line accordingly.
- **Lender interest is informational only.** Loans the user has
  extended display `+N interest receivable at repay` for transparency,
  but the principal is already locked in their cash position and the
  interest hasn't accrued there yet — neither feeds the liability
  totals.

### Phase 4 v1c: governance-mutable haircut table (`RiskParams`)

The collateral haircut table is now an on-chain PDA owned by
`ssr-compliance`. `margin show` reads it on every call, so a haircut
change made by the super-admin is reflected the next time anyone runs
the view — no CLI restart, no deploy.

```sh
# One-time bootstrap after deployment. Super-admin signs (= the
# wallet that ran `compliance init-registry`).
ssr-cli compliance init-risk-params
# risk_params @ <PDA>
#   version            = 1
#   bump               = 254
#   last_modified_slot = <slot>
#   haircut_bps:
#     [  1] TOKENIZED_DEPOSIT    =     0 bps  (default)
#     [  2] STABLECOIN           =     0 bps  (default)
#     [  3] SOVEREIGN_BOND       =   500 bps  (default)
#     [  4] CORPORATE_BOND       =  1500 bps  (default)
#     [  5] EQUITY               =  3000 bps  (default)
#     [  6] FUND_UNIT            =  2000 bps  (default)
#     [  7] REAL_ASSET           =  4000 bps  (default)
#     [  8] COMMODITY            =  2500 bps  (default)

# Governance vote / risk-committee decision → tighten equity to 45%.
ssr-cli compliance set-haircut --class equity --bps 4500
# risk_params @ <PDA>
#   ...
#     [  5] EQUITY               =  4500 bps  (was default 3000)
#     ...

# Read-only inspection (no keypair needed).
ssr-cli compliance show-risk-params
```

`margin show` falls back to `ssr_types::default_haircut_bps` when the
PDA isn't allocated (so the demo dramaturgy from `cli/README.md` still
works on a fresh deployment that hasn't run `init-risk-params` yet).
The view emits an explicit banner in either case so an operator
never mistakes a stale read for the live policy.

Authorization, validation, and error namespaces:

- **`init-risk-params`** and **`set-haircut`** both require the global
  `--keypair` to match `Registry::super_admin`. The compliance program
  rejects others with `UNAUTHORIZED_SUPER_ADMIN` (`0x1021`).
- **`set-haircut`** validates `bps <= 10_000` (else `HAIRCUT_OUT_OF_RANGE`
  / `0x1033`) and `class < 32` (else `ASSET_CLASS_OUT_OF_RANGE`
  / `0x1032`). The PDA's `haircut_bps` is a fixed `[u16; 32]`; index 0
  is `UNKNOWN` (default 10_000 bps so a fresh / mistyped class never
  silently inflates margin).
- **`init-risk-params`** is not idempotent at the protocol level —
  re-running rejects via the system program's `AccountAlreadyInitialized`.
  Run once per deployment and then update individual cells with
  `set-haircut`.

### Phase 4 v1b: on-chain margin enforcement in `open_loan`

`ssr-lending::open_loan` now rejects on chain when the borrower would
be undercollateralized after the loan. Off-chain `margin show` from
v1a still shows the projected number; v1b is the *enforcement* of
that number at the gate.

```sh
# Pre-req (one-time per deployment): the governance-mutable haircut
# table must exist. v1b's margin check reads from it on every open.
ssr-cli compliance init-risk-params

# Open as usual. The CLI auto-discovers the borrower's open loans
# (via the `LoanList` PDA) and passes them to the handler so the
# margin gate sees the full liability set:
ssr-cli lending open --borrower-keypair ... --lender-keypair ... \
                     --collateral-mint $MINT_ASSET --cash-mint $MINT_CASH \
                     --collateral-amount 300000 --principal-amount 200000 \
                     --maturity-slot $((current_slot + 10000)) --nonce 1 \
                     [--interest-bps-per-year 500]

# If the borrower lacks collateral headroom:
# Error: tx failed: Custom(0x5035)   ← MARGIN_INSUFFICIENT
```

Model — conservative cross-margin, no FX:

- **Pool** = Σ over the borrower's collateral positions of
  `position.available × (10_000 − RiskParams.haircut_bps[asset_class]) / 10_000`,
  minus `collateral_amount × collateral_credit` (the new lock).
  The new cash drawdown does NOT add to the pool — the borrower can
  withdraw it from their cash position and leave the obligation
  behind, so counting it would make the gate trivially pass.
- **Existing liabilities** = Σ over `LoanList` entries of
  `loan.principal_amount + simple_interest(principal, bps,
  maturity − opened)` (projected to each loan's own maturity).
- **New liability** = `principal_amount + simple_interest(principal,
  bps_per_year, new_maturity − now)`.
- **Reject** with `MARGIN_INSUFFICIENT` (`0x5035`) when
  `pool < existing_liabilities + new_liability`.

`LoanList` — adversarial defense:

The handler maintains a per-borrower `LoanList` PDA (one per
borrower, at `[b"loan-list", borrower]`). Every `open_loan` appends
the new loan's PDA; every `repay_loan` / `liquidate_loan` removes it.
The margin gate requires the caller to pass exactly the set of
`Loan` PDAs in `LoanList` — no more, no less. This blocks the
"omit-a-liability" adversarial pattern that any pure pass-account
gate is vulnerable to. `MAX_ENTRIES = 16` simultaneous open loans
per borrower (caps both rent and the on-tx account budget under
Solana's ~64-key limit; further opens reject with `LOAN_LIST_FULL`).

Other v1b errors operators may see:

- `LOAN_LIST_PDA_MISMATCH` (`0x5030`) — passed account isn't the
  borrower's `LoanList` PDA.
- `LOAN_LIST_LAYOUT_INVALID` (`0x5031`) — data shorter than
  `LoanList::LEN`. Surfaces if the PDA is stale from a pre-v1b
  layout.
- `LOAN_LIST_BORROWER_MISMATCH` (`0x5032`) — caller passed someone
  else's `LoanList`.
- `LOAN_LIST_FULL` (`0x5033`) — borrower at `MAX_ENTRIES`; must
  repay or be liquidated before opening another.
- `LOAN_NOT_IN_LIST` (`0x5034`) — `repay_loan` / `liquidate_loan`
  invoked on a `Loan` not present in the borrower's `LoanList`.
  Indicates either list corruption or a pre-v1b loan being touched.
- `MARGIN_INSUFFICIENT` (`0x5035`) — margin gate rejection. See
  `ssr-cli margin show` for the per-cash-mint breakdown.
- `RISK_PARAMS_PDA_MISMATCH` (`0x5036`) — `RiskParams` not
  initialized (or wrong PDA passed). Run
  `compliance init-risk-params` first.
- `MARGIN_POSITION_MISMATCH` (`0x5037`) — a passed position PDA is
  not the borrower's, or vault PDA doesn't match the position's
  `vault` field, or duplicate position (extra-positions list must be
  in strictly ascending pubkey order).
- `MARGIN_LOAN_SET_MISMATCH` (`0x5038`) — the existing-loan PDAs
  passed don't match `LoanList` exactly (omission, substitution, or
  duplication).

Notes and limits:

- **No cross-mint cross-margin yet.** A position in mint A contributes
  haircut-credit to the pool regardless of the new loan's cash mint
  — i.e., the haircut acts as implicit FX. An oracle-backed model
  is reserved for a later sub-phase.
- **No multi-lock Position.** Phase 3's single-`lock_authority` rule
  is still in force, so a single collateral position can back at
  most one open loan at a time. To open multiple loans, deposit into
  multiple distinct vaults.
- **No interest-rate variation.** Loans are still simple-interest
  with the rate fixed at open. Variable-rate loans are out of v1b.

### Phase 4 v1d: oracle-priced cross-margin (`PriceFeed`)

v1b's gate treated haircut as implicit FX — fine for stablecoin-cash
loans against stablecoin-class collateral, but it couldn't say
anything meaningful about, say, an EQUITY-class collateral position
backing a USD loan. v1d makes the FX explicit: each priced mint gets
a `PriceFeed` PDA (price in micro-USD per native unit + mint
decimals), the lending gate reads them on every `open_loan` and
compares pool and liabilities in a shared numéraire (micro-USD).

```sh
# One-time deployment setup: super-admin registers a PriceFeed for
# every mint that will be involved in margin. The price + decimals
# are captured here; only the price is mutable later.
ssr-cli compliance register-price-feed \
    --mint $MINT_COLLATERAL --price-micro-usd 1000000 --mint-decimals 6
ssr-cli compliance register-price-feed \
    --mint $MINT_CASH --price-micro-usd 1000000 --mint-decimals 6

# (Optional) Rotate the oracle role away from super-admin so a
# bot/cron job can refresh prices without holding the multisig key.
ssr-cli compliance rotate-operator --role oracle --new-pubkey $ORACLE_BOT

# Cadence: the oracle operator updates each feed on whatever
# schedule is appropriate for the asset. Stale prices reject — see
# the staleness gate below.
ssr-cli compliance update-price --mint $MINT_COLLATERAL --price-micro-usd 1050000
# → SOVEREIGN_BOND moved from $1.000 to $1.050

# Read-only inspection.
ssr-cli compliance show-price-feed --mint $MINT_COLLATERAL
# price_feed @ <PDA>
#   mint                = <MINT_COLLATERAL>
#   price_micro_usd     = 1050000 ($ 1.050000)
#   mint_decimals       = 6
#   last_updated_slot   = 1234567

# Staleness gate (governance-tunable). Zero disables it (the pre-v1d
# default). Real deployments should set it to a value matched to the
# slowest-cadence feed in the system.
ssr-cli compliance set-max-staleness --slots 600  # ~4 min at mainnet cadence
```

After registration, `ssr-cli lending open` automatically discovers
the PriceFeeds for the new loan's collateral mint and cash mint,
sorts their PDAs ascending, and appends them to the trailing
accounts (v1d's `price_feed_count = 2`). It also surfaces a clear
error if either feed is missing or RiskParams isn't initialized
— the operator gets a "run `register-price-feed` first" pointer
rather than a mysterious on-chain reject.

Model (in micro-USD):

- **Pool** = Σ over the borrower's haircut-adjusted positions of
  `balance × price / 10^decimals × (10_000 − haircut) / 10_000`,
  minus the new lock at full collateral credit (same conservative
  exclusion of the new drawdown as v1b).
- **Liabilities** = Σ over `LoanList` entries of `(principal +
  interest_to_maturity) × cash_price / 10^cash_decimals`, plus the
  new loan's `(principal + interest_to_maturity) × cash_price /
  10^cash_decimals`. No haircut on the liability side — a dollar
  owed is a dollar owed.
- **Reject** with `MARGIN_INSUFFICIENT` when `pool < liabilities`.

v1d-specific limits to be aware of:

- **Single cash mint per borrower** (until v1e). The gate requires
  every entry in the borrower's `LoanList` to share the new loan's
  `cash_vault` — mismatched cash mints reject with
  `MARGIN_LOAN_SET_MISMATCH`. Practical effect: a borrower can take
  loans in one settlement currency (e.g., USDC) backed by collateral
  in *any* haircut-credited mint, but they can't simultaneously hold
  a USDC loan and a EURC loan. Multi-cash-mint comes when each
  loan's `cash_vault` is also passed in the trailing accounts so the
  handler can find each loan's matching feed.
- **Mock oracle only**. v1d uses the manual `update_price` flow —
  there's no on-chain Pyth/Switchboard adapter yet. The
  `oracle_operator` role is the trust point; rotate it to whatever
  signer your price-refresh job uses.
- **No re-pricing of in-flight transactions**. The gate reads the
  PriceFeed at handler-execution time. If the price changes mid-
  block (a separate `update_price` lands in the same slot but
  earlier), the gate sees the new value. There's no on-chain
  guarantee that a quoted price stays valid through the operator's
  RPC round-trip.
- **No price impact / liquidity modelling**. The haircut is the
  catch-all for "what fraction of nominal can we realize in a
  liquidation". A finer model (per-mint liquidity curves, slippage
  caps, time-to-liquidate) is out of scope.

v1d error codes (lending namespace):

- `PRICE_FEED_MISSING` (`0x5039`) — a mint in scope has no PriceFeed
  passed (or registered). Make sure the CLI knows about every mint
  involved.
- `PRICE_FEED_STALE` (`0x503A`) — a PriceFeed's `last_updated_slot`
  is older than `RiskParams.max_staleness_slots`. Either refresh the
  price or widen the staleness window.
- `PRICE_FEED_ORDER_INVALID` (`0x503B`) — the price-feed slice
  wasn't in strictly-ascending pubkey order (a caller-side bug;
  `ssr-cli` sorts automatically).
- `PRICE_OVERFLOW` (`0x503C`) — pathological `balance × price`
  product overflowed u128. Real-world inputs won't hit this.
- `PRICE_FEED_PDA_MISMATCH` (`0x503D`) — a passed feed account isn't
  the PDA derived from `[seeds::PRICE_FEED, mint] @ compliance_program`,
  or it's owned by the wrong program.

v1d compliance namespace additions:

- `PRICE_FEED_PDA_MISMATCH` (`0x1040`), `PRICE_FEED_LAYOUT_INVALID`
  (`0x1041`), `UNAUTHORIZED_ORACLE` (`0x1042`) — surface from
  `register_price_feed` / `update_price` failures.

### Phase 4 v1e: multi-cash-mint cross-margin

v1d's gate had a single-cash-mint constraint: every entry in a
borrower's `LoanList` had to share the new loan's `cash_vault`. v1e
drops that. Each existing loan's `cash_vault` is passed in a
parallel slice alongside the loan PDA; the handler reads each
vault to find its mint, looks up the matching `PriceFeed`, and
computes that loan's liability in micro-USD. The pool is unchanged
— still cross-collateral haircut-adjusted USD — so a borrower can
hold simultaneous loans in different settlement currencies off the
same collateral.

```sh
# Pre-req (per deployment): register PriceFeeds for every mint that
# will be a loan's cash leg. Same flow as v1d:
ssr-cli compliance register-price-feed --mint $MINT_USDC \
    --price-micro-usd 1000000 --mint-decimals 6
ssr-cli compliance register-price-feed --mint $MINT_EURC \
    --price-micro-usd 1080000 --mint-decimals 6    # €1.00 = $1.08

# Open USDC loan first (locks borrower's sovereign_A collateral
# against loan_1). Standard v1d flow — CLI auto-discovers the cash
# mint's feed.
ssr-cli lending open --borrower-keypair ... --lender-keypair ... \
                     --collateral-mint $MINT_SOVEREIGN_A --cash-mint $MINT_USDC \
                     --collateral-amount 300000 --principal-amount 200000 \
                     --maturity-slot $((cur + 10000)) --nonce 1

# Open EURC loan second, using a DIFFERENT collateral position
# (sovereign_B, since Phase 3 limits one lock per Position). The
# CLI auto-discovers loan_1 in the borrower's LoanList, reads its
# cash_vault to find MINT_USDC, and adds MINT_USDC's PriceFeed
# alongside MINT_EURC's. The handler computes both liabilities in
# micro-USD and rejects if the cross-mint total exceeds the pool.
ssr-cli lending open --borrower-keypair ... --lender-keypair ... \
                     --collateral-mint $MINT_SOVEREIGN_B --cash-mint $MINT_EURC \
                     --collateral-amount 300000 --principal-amount 180000 \
                     --maturity-slot $((cur + 10000)) --nonce 2
```

What v1e adds to the on-chain account layout (`open_loan` trailing
section):

```
[16 fixed accounts]
[N × 2 (position, vault) pairs]              -- v1b: extra positions
[M loan PDAs]                                -- v1b: existing loans
[M cash_vault PDAs]                          -- v1e: parallel to loans
[P PriceFeed PDAs (strictly-ascending)]      -- v1d: mint price oracles
```

The CLI computes all four sections automatically — operators just
pass `--collateral-amount` / `--principal-amount` / etc. as before.

What v1e does NOT change:

- **Single-lock Position.** Each loan still ties up one collateral
  position; to run two simultaneous loans, the borrower needs two
  collateral mints (or two different collateral *vaults* under
  multi-lock Position when Phase 5 lands).
- **Pool aggregation model.** Pool is still Σ haircut-adjusted
  collateral USD; the new flexibility is on the liability side.
- **Account budget.** With `LoanList::MAX_ENTRIES = 16` and worst-
  case multi-cash-mint, the open_loan tx adds ~16 (cash_vaults) +
  ~17 (distinct mint feeds) accounts on top of v1d. Total at ~64,
  brushing Solana's hard limit. Borrowers running close to the cap
  may need to repay before opening fresh loans.

No new error codes — v1e reuses `MARGIN_LOAN_SET_MISMATCH`
(`0x5038`) for the per-loan cash_vault validation (caller passes
the wrong vault for a loan in the list).

### Phase 4 v1f: Pyth oracle adapter

v1a–v1e built the gate around an internal `PriceFeed` PDA, updated
manually by the `oracle_operator`. v1f wraps Pyth: super-admin binds
a feed to a specific Pyth `PriceUpdateV2` account, then the oracle
operator (or a refresh bot under that role) calls
`update_price_from_pyth`, which reads Pyth's on-chain price,
applies `price − conf` for conservative pricing, normalizes the
exponent to micro-USD, and writes the result into the existing
cached field. The downstream lending gate is unchanged — it still
reads `PriceFeed.price_micro_usd` and compares against
`max_staleness_slots`.

```sh
# One-time per (mint, Pyth feed): bind the SSR PriceFeed to the
# real Pyth account. Super-admin signs.
ssr-cli compliance bind-price-feed-to-pyth \
    --mint $MINT_USDC \
    --pyth-source $PYTH_PRICE_UPDATE_V2_FOR_USDC

# Refresh from Pyth. Oracle operator signs. The CLI reads the
# binding from the PriceFeed itself, so no --pyth-source flag.
ssr-cli compliance update-price-from-pyth --mint $MINT_USDC
# → reads Pyth's price + conf + exponent, conservative-adjusts,
# → writes the new micro-USD value, bumps last_updated_slot

# show-price-feed displays the binding alongside the cached price:
ssr-cli compliance show-price-feed --mint $MINT_USDC
# price_feed @ <PDA>
#   ...
#   pyth_source         = <PYTH_PRICE_UPDATE_V2_FOR_USDC> (Pyth-bound)
```

Layout (Pyth's `PriceUpdateV2`, 134 bytes, Anchor-style):

```
[  0..  8] discriminator  = sha256("account:PriceUpdateV2")[..8]
[  8.. 40] write_authority: Pubkey
[ 40.. 42] verification_level: 1-byte tag + 1-byte payload
[ 42.. 74] feed_id: [u8; 32]
[ 74.. 82] price:        i64 LE   ← SSR reads
[ 82.. 90] conf:         u64 LE   ← SSR reads
[ 90.. 94] exponent:     i32 LE   ← SSR reads
[ 94..134] timestamps + ema fields (unused by SSR)
```

Trust + safety design:

- **No Pyth program-ID validation.** SSR doesn't hard-code the
  Pyth Receiver program ID. The super-admin is the trust point —
  they're responsible for binding to a real Pyth account at
  `bind_price_feed_to_pyth` time. After that, `update_price_from_pyth`
  asserts the passed Pyth account matches the bound pubkey exactly
  (`PYTH_SOURCE_MISMATCH`).
- **Discriminator pinned at the byte level.** `[34, 241, 35, 99,
  157, 126, 244, 205]`. If Pyth migrates to `PriceUpdateV3` (or
  changes the discriminator scheme), updates reject with
  `PYTH_ACCOUNT_INVALID`. A test in `ssr-compliance::smoke`
  recomputes the discriminator from `sha256("account:PriceUpdateV2")`
  and asserts equality — change-detection on Pyth's renaming.
- **Conservative pricing.** Quoted price is `pyth.price -
  pyth.conf`. If `conf > price` or `price <= 0`, the handler
  rejects with `PYTH_NEGATIVE_PRICE` rather than clamping to zero
  — silent zero would let a broken / under-attack feed continue
  driving margin decisions.
- **Exponent normalization.** Pyth uses `(value, exponent)` with
  the actual price = `value × 10^exponent`. SSR converts to micro-USD
  via `value × 10^(exponent + 6)`. The handler accepts `exponent +
  6 ∈ [-12, 6]` (covering all real Pyth feeds today) and rejects
  anything outside with `PYTH_EXPONENT_OUT_OF_RANGE` rather than
  silently overflowing or under-flowing.
- **Manual fallback retained.** `update_price` still works on a
  Pyth-bound feed. If Pyth is down or the oracle operator can't
  reach a node, the super-admin can rotate the role and manually
  push a price — `update_price_from_pyth` is opt-in, not exclusive.

v1f error codes (compliance namespace):

- `PRICE_FEED_NOT_PYTH_BOUND` (`0x1043`) — the feed's
  `pyth_source` is still `[0; 32]`; run
  `bind-price-feed-to-pyth` first.
- `PYTH_SOURCE_MISMATCH` (`0x1044`) — passed Pyth account doesn't
  match the bound source. Most likely a CLI bug or a deliberate
  substitution attempt.
- `PYTH_ACCOUNT_INVALID` (`0x1045`) — Pyth account's
  discriminator or data length is wrong. Pyth may have migrated
  layouts, or the bound account isn't actually Pyth.
- `PYTH_NEGATIVE_PRICE` (`0x1046`) — `pyth.price ≤ 0` or `conf >
  price` after subtraction. The feed is broken / under attack;
  don't update.
- `PYTH_EXPONENT_OUT_OF_RANGE` (`0x1047`) — Pyth's exponent
  would yield a `10^N` factor outside `[-12, 6]`. Investigate the
  feed; this should never happen for real assets.

### Phase 4 v1g: Pyth account owner-validation

v1f's trust model leaned on the super-admin's bind: whatever account
they pointed `bind_price_feed_to_pyth` at, the gate would later read
prices from. If the super-admin's wallet got phished or their
machine compromised, an attacker could bind a feed to a fake
"Pyth-formatted" account they controlled and feed wrong prices into
the margin gate. v1g closes that hole by validating the Pyth
account's owner program ID against a value the super-admin sets
once via `initialize_pyth_config`.

The check is **opt-in via PDA existence**: if `PythConfig` isn't
allocated, the program falls back to v1f behavior (no owner check).
Once any super-admin runs `init-pyth-config`, the CLI and on-chain
handlers both start including the PDA in every bind / update call —
the check becomes mandatory until governance unbinds (currently no
"uninit" instruction; that's deliberate, since rolling back the
check is itself a security-sensitive event).

```sh
# One-time deployment setup. The Pyth Receiver program ID is
# `rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ` on Solana
# mainnet/devnet; private deployments use whatever their oracle
# source is.
ssr-cli compliance init-pyth-config \
    --pyth-program-id rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ

# Subsequent bind/update calls auto-attach the PythConfig PDA so the
# on-chain handler validates the Pyth account's owner. Operators
# see no flag change — the CLI handles it transparently:
ssr-cli compliance bind-price-feed-to-pyth --mint $MINT_USDC \
    --pyth-source $PYTH_PRICE_UPDATE_V2_FOR_USDC
ssr-cli compliance update-price-from-pyth --mint $MINT_USDC

# Inspect: the show command makes the deployment's mode visible.
ssr-cli compliance show-pyth-config
# pyth_config @ <PDA>
#   pyth_program_id     = rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ
#   ...

# If Pyth ever redeploys under a new program ID:
ssr-cli compliance set-pyth-program-id --pyth-program-id <new-pid>
```

Trust + safety design notes:

- **PythConfig is one global record per deployment**, not per
  feed. Mixing oracle providers (some mints from Pyth, others
  from a different program) is out of v1g scope.
- **The check is on the Pyth account's `owner`**, not on its data.
  An attacker who controls the Pyth program could still publish
  arbitrary prices, but that's a much higher bar than just owning
  the super-admin key.
- **Unbinding skips the check.** When `bind-price-feed-to-pyth`
  is called with the all-zero pubkey (disabling Pyth refresh for a
  feed), no owner validation runs — there's no Pyth account to
  validate. The feed simply falls back to manual `update-price`.
- **v1f tests stay green.** They don't allocate `PythConfig`, so
  the on-chain handlers take the legacy path. v1g doesn't break
  any prior deployment until the operator explicitly opts in.

v1g error code (compliance namespace):

- `PYTH_PROGRAM_ID_MISMATCH` (`0x104A`) — the passed Pyth
  account's owner doesn't match `PythConfig.pyth_program_id`.
  Either someone's trying to bind/refresh against a fake feed, or
  Pyth has redeployed under a new program ID and the config needs
  `set-pyth-program-id` to update.

Also: `PYTH_CONFIG_PDA_MISMATCH` (`0x1048`) and
`PYTH_CONFIG_LAYOUT_INVALID` (`0x1049`) for the standard PDA-shape
guards on the config account itself.

### Error namespaces (operator triage)

The error namespace makes the failure source unambiguous in operator
logs:

| Range  | Source                     |
|--------|----------------------------|
| 0x10XX | `ssr-compliance`           |
| 0x20XX | `ssr-dvp-wrapper`          |
| 0x30XX | `ssr-vault`                |
| 0x40XX | `ssr-repo`                 |
| 0x50XX | `ssr-lending`              |

## What the CLI does NOT cover

- **Token-2022 mint creation + minting.** Use `spl-token` for that;
  there is no SSR-specific behavior at issuance other than choosing
  whether to attach TransferHook (Model A) or not (Model C).
- **SPC CreateDvp.** Permissionless per SPC's spec; use any SPC client
  or the generated IDL. The wrapper only adds value at `Settle` time.
- **Channel-internal operations.** If you run an SPC channel (Model B
  or C), channel-side operations are handled by SPC's own surface.

## Tips for live demos

- Run with `RUST_LOG=info` (uncolored) for clean output that screenshots well.
- Pre-derive everything: print `ssr-cli derive registry`, `ssr-cli dvp
  authority-address`, and the canonical `swap-dvp` address before
  the demo so the attendees see what's happening.
- Use `ssr-cli compliance status` between every state-changing command
  to make the gate visible. The "transfer gate" line is the punchline.
- For the negative path, demonstrating that `suspend` fires the
  distinct `0x2013` error (rather than a generic reject) is the
  consulting-value differentiator vs. an opaque rejection from
  Token-2022 or SPC alone.
