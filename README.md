# PERMAFROST

<img src="src/brick-firn.svg" width="120" align="right" alt="The firn brick — PERMAFROST logo">

A perpetual liquidity vault — a tontine machine on ALKANES. Wraps any
alkane token (canonically the BTCUSD LP) into a single share with no
terms: enter at the rate, exits pay a tithe, the tithe drips back to
those who stay. Immutable: no proxy, no admin, no protocol fee.

Companion documents: whitepaper v0.16, invariant specification v0.5+.

## Mechanics

State: `L` (the vault's underlying, a storage counter), `B` (the pot),
`S` (shares), `h₀` (drip anchor). Share rate `c = (L − B) / S`.
Instance parameters: `I` — blocks per drip, `P` — drips to empty the
pot (canonically 144 and 365).

```
release (the first step of every mutation):
  e = ⌊(h − h₀)/I⌋ ;  B ← B·((P−1)/P)^e ;  h₀ ← h₀ + I·e
deposit d:  mint = ⌊d·S/(L−B)⌋ ;  L ← L + d
exit b:     W = ⌊b·(L−B)/S⌋ ;  π = ⌈p·W⌉ ;  payout = W − π
            L ← L − payout ;  B ← B + π
```

- Deposits and exits do not move the rate; the underlying's own yield
  (for the canonical LP — pool fees inside the token) and the drips push
  it up; nothing pushes it down.
- The last exit follows the common rule; the pot survives an exodus as a
  dowry: while only dead shares stand, the release is frozen (epochs are
  consumed, the pot stays whole) and thaws with the first new depositor.
- Every rounding works against the initiator. The first deposit burns
  1000 dead shares (`S = 0` becomes unreachable) and must exceed 1000
  units.

## Opcodes

| # | Call | Attach | Returns |
|---|---|---|---|
| 0 | `Initialize{underlying, penalty_bps, name, symbol, drip_interval, release_periods}` | — | once; everything immutable after |
| 1 | `Deposit` | underlying | shares; data = mint (u128 LE) |
| 2 | `Withdraw` | shares | underlying; data = payout, tithe |
| 3 | `DonatePot` | underlying | tribute into the pot — drips over the horizon; allowed always (pre-genesis it waits frozen) |
| 4 | `DonateBoost` | underlying | instant, up-only rate lift; requires standers, otherwise reverts with a refund; submit privately |
| 99/100 | `GetName` / `GetSymbol` | — | string |
| 101/102/107 | `GetTotalShares` / `GetVaultBalance` / `GetPot` | — | S / L / B |
| 103/104 | `GetPenaltyBps` / `GetUnderlying` | — | p / AlkaneId (32 bytes LE) |
| 105 | `QuoteDeposit{amount}` | — | mint (virtual release applied) |
| 106 | `QuoteWithdraw{shares}` | — | payout, tithe |
| 108 | `GetState` | — | L, B, S (48 bytes, never reverts) |
| 109/110/111 | `GetPotAnchor` / `GetDripInterval` / `GetReleasePeriods` | — | h₀ / I / P |
| 1000 | `GetData` | — | logo (SVG, embedded in the bytecode — identical for every clone) |

Clients project pending drips themselves: `e = ⌊(h − h₀)/I⌋`,
`B_now = B·((P−1)/P)^e`.

## Initialization

Cellpack: `[0, underlying_block, underlying_tx, penalty_bps, name,
symbol, interval, periods]`.

- `name`/`symbol` — strings of ≤ 16 bytes packed into a u128 as LE bytes
  (the alkanes standard; zero bytes are dropped).
- Canon: `penalty_bps = 1000` (10%), `interval = 144` (Bitcoin's day),
  `periods = 365` (the tithe arrives within a year). Bounds:
  `bps ≤ 10000`, `interval ≥ 1`, `periods ≥ 1`.
- A shorter horizon weakens the forfeit: an exiter loses on average
  `periods` drips of other people's pot. Drip sniping stays dead as long
  as `pot/P ≪ penalty·vault`.

## Build & test

Prerequisites: Rust with the `wasm32-unknown-unknown` target and
`wasm-bindgen-cli` 0.2.100 (`cargo install -f wasm-bindgen-cli --version
0.2.100`) for the test runner.

The vault's test WASM is a gitignored build artifact; a fresh clone
must produce it once before the first test run:

```bash
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/permafrost.wasm src/tests/wasm/
cargo test --target wasm32-unknown-unknown
```

`fire_master_auth.wasm` is a committed test fixture: the simplest
mintable alkane (built from the subfrost/fire monorepo), deployed by the
harness as the mock underlying token.
The test suite is self-contained (the harness and vendored WASMs live
inside the crate) and asserts every on-chain state against an exact
integer replica (`Mirror`) — for equality, not tolerance.

## Sharp edges

- **Underlying sent by edict outside the opcodes is lost forever** —
  there is no sweep and no admin. Tribute must use
  DonatePot/DonateBoost.
- Always set pointer/refund (`:v0:v0`) in cellpacks so reverts refund
  the tokens.
- Deposit/Withdraw consume the **entire** attached transfer of the
  target token; unrelated tokens in the same transaction are returned
  automatically.
- Submit `DonateBoost` privately: same-block entrants share the gift
  pro-rata. A public race for a boost is economically dead — a
  round-trip around any boost under ~11% of the vault loses to the
  tithe.
- Long silence is safe: the release is lazy, O(log e) — an exit after a
  thousand quiet years costs ~213k fuel against the 3.5M budget.
