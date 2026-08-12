//! PERMAFROST — a perpetual vault for any fungible alkane token (a tontine).
//! Deposits mint the vault share at the current rate; exits pay a tithe
//! into THE POT; the pot drips back to those who stay. Immutable: no
//! proxy, no admin, no protocol fee. Cloneable: each instance binds its
//! underlying, parameters, name and logo at initialization.
//!
//! This header states only what the code must uphold — usage, the opcode
//! table and deploy notes live in README.md.
//!
//! State: L (vault liquidity — a storage counter of the underlying, not
//! the runtime balance), B (the pot), S (shares), h₀ (drip anchor).
//! Share rate c = (L − B)/S. Per-instance parameters, immutable after
//! init: tithe p (bps), drip interval I (blocks), release horizon P
//! (drips). The reference configuration is 10%, 144, 365.
//!
//! Semantics:
//! - release — the first state step of every mutating op:
//!     e = ⌊(h − h₀)/I⌋;  B ← B·((P−1)/P)^e;  h₀ ← h₀ + I·e
//!   Fixed-point power, O(log e). Frozen while S ≤ DEAD_SHARES: epochs
//!   are consumed, nothing is released to unburnable shares.
//! - deposit d:      mint = ⌊d·S/(L−B)⌋;  L ← L + d
//! - exit b:         W = ⌊b·(L−B)/S⌋;  π = ⌈p·W⌉;  payout = W − π;
//!                   L ← L − payout;  B ← B + π  (same rule for the last
//!                   exit — the pot survives an exodus as a dowry)
//! - donate-pot d:   L ← L + d;  B ← B + d  (allowed always; pre-genesis
//!                   tribute waits frozen)
//! - donate-boost d: L ← L + d  (instant, up-only; requires standers,
//!                   otherwise reverts and refunds)
//!
//! Invariants the code upholds:
//! - every rounding goes against the initiator: mint and W floor, the
//!   tithe ceils, the release floors;
//! - L′ + payout = L exactly, in integers (conservation);
//! - the rate never decreases; deposits and exits leave it untouched;
//! - genesis burns DEAD_SHARES — S = 0 is unreachable afterwards;
//! - the critical path makes no external calls and reads no oracles; the
//!   only external interaction is the underlying transfer itself;
//! - underlying sent by edict outside these opcodes is not credited to L
//!   and is lost forever (no admin, no sweep — documented, not a bug).

// Under the test profile the contract entrypoint (declare_alkane) is not
// compiled — tests drive the vendored WASM through the metashrew VM — so
// the whole opcode surface is "dead" to rustc. Not in the release build.
#![cfg_attr(test, allow(dead_code))]

use alkanes_runtime::{
    message::MessageDispatch, runtime::AlkaneResponder, storage::StoragePointer,
};
#[cfg(not(test))]
use alkanes_runtime::declare_alkane;
use alkanes_support::{
    context::Context, id::AlkaneId, parcel::AlkaneTransfer, response::CallResponse,
};
use anyhow::{anyhow, ensure, Result};
#[cfg(not(test))]
use metashrew_support::compat::to_arraybuffer_layout;
use metashrew_support::index_pointer::KeyValuePointer;
use ruint::Uint;
use std::sync::Arc;

type U256 = Uint<256, 4>;

/// Basis-point denominator: penalty_bps = 1000 → the reference 10% tithe.
const BPS: u128 = 10_000;

/// Genesis shares burned on the first deposit. Never circulate, keep S > 0
/// forever, and bound the dust the last exit leaves behind.
const DEAD_SHARES: u128 = 1_000;

/// Fixed-point scale for the decay power ((P−1)/P)^e.
const FP: u128 = 1 << 64;

/// Decode a u128-packed string (LE bytes, zero bytes dropped) — the
/// standard alkanes token-name packing, same as alkanes-std-factory-support.
fn trim(v: u128) -> String {
    String::from_utf8(
        v.to_le_bytes()
            .into_iter()
            .filter(|b| *b != 0)
            .collect::<Vec<u8>>(),
    )
    .unwrap_or_default()
}

#[derive(Default)]
pub struct Permafrost(());

impl AlkaneResponder for Permafrost {}

#[derive(MessageDispatch)]
enum PermafrostMessage {
    /// One-time setup: bind the underlying token; fix the tithe rate, the
    /// drip cadence (blocks per drip; reference: 144) and the release
    /// horizon (drips to empty the pot ~once; reference: 365); set the
    /// share token's name/symbol (one u128 each, LE bytes, the standard
    /// alkanes packing) — all immutable afterwards
    #[opcode(0)]
    Initialize {
        underlying: AlkaneId,
        penalty_bps: u128,
        name: u128,
        symbol: u128,
        drip_interval: u128,
        release_periods: u128,
    },

    /// Deposit the underlying token from incoming alkanes, mint shares at the current rate
    #[opcode(1)]
    Deposit,

    /// Burn incoming shares, withdraw the underlying minus the tithe (→ the pot)
    #[opcode(2)]
    Withdraw,

    /// Donate the underlying into the pot — the default tribute road: drips to
    /// standers over the year (L += d, B += d; the rate does not jump)
    #[opcode(3)]
    DonatePot,

    /// Donate the underlying straight into the rate — the campaign road: instant,
    /// up-only, for those standing now (L += d; submit privately)
    #[opcode(4)]
    DonateBoost,

    /// Get token name
    #[opcode(99)]
    #[returns(String)]
    GetName,

    /// Get token symbol
    #[opcode(100)]
    #[returns(String)]
    GetSymbol,

    /// Get total share supply S (includes dead shares)
    #[opcode(101)]
    #[returns(u128)]
    GetTotalShares,

    /// Get the vault liquidity counter L (includes the pot)
    #[opcode(102)]
    #[returns(u128)]
    GetVaultBalance,

    /// Get the tithe rate in basis points
    #[opcode(103)]
    #[returns(u128)]
    GetPenaltyBps,

    /// Get the underlying token AlkaneId (32 bytes LE: block, tx)
    #[opcode(104)]
    #[returns(Vec<u8>)]
    GetUnderlying,

    /// Quote: shares minted for a deposit of `amount` (virtual release applied)
    #[opcode(105)]
    #[returns(u128)]
    QuoteDeposit { amount: u128 },

    /// Quote: (payout, tithe) for burning `shares` (two u128 LE, virtual release applied)
    #[opcode(106)]
    #[returns(Vec<u8>)]
    QuoteWithdraw { shares: u128 },

    /// Get the pot B (stored value — pending drips not applied)
    #[opcode(107)]
    #[returns(u128)]
    GetPot,

    /// Get raw state (L, B, S) — three u128 LE, pure storage read, never
    /// reverts; project pending drips client-side via opcodes 109–111
    #[opcode(108)]
    #[returns(Vec<u8>)]
    GetState,

    /// Get the drip anchor h₀ (block height as u128 LE)
    #[opcode(109)]
    #[returns(u128)]
    GetPotAnchor,

    /// Get the drip interval in blocks (reference: 144)
    #[opcode(110)]
    #[returns(u128)]
    GetDripInterval,

    /// Get the release horizon in drips (reference: 365)
    #[opcode(111)]
    #[returns(u128)]
    GetReleasePeriods,

    /// Get the token image (SVG bytes) — the standard alkanes data opcode
    #[opcode(1000)]
    #[returns(Vec<u8>)]
    GetData,
}

/// ⌊a·b/d⌋ via U256 — hard error on u128 overflow of the quotient.
fn muldiv_floor(a: u128, b: u128, d: u128) -> Result<u128> {
    ensure!(d != 0, "division by zero");
    (U256::from(a) * U256::from(b) / U256::from(d))
        .try_into()
        .map_err(|_| anyhow!("muldiv overflow"))
}

/// ⌈a·b/d⌉ via U256 — hard error on u128 overflow of the quotient.
fn muldiv_ceil(a: u128, b: u128, d: u128) -> Result<u128> {
    ensure!(d != 0, "division by zero");
    let d256 = U256::from(d);
    ((U256::from(a) * U256::from(b) + d256 - U256::from(1u8)) / d256)
        .try_into()
        .map_err(|_| anyhow!("muldiv overflow"))
}

/// ⌈a·b/FP⌉ for fixed-point factors in [0, FP]. Ceiling keeps the power an
/// over-approximation of the true ((P−1)/P)^e, so the released amount only
/// ever rounds DOWN — the remainder stays in the pot, against no one.
fn mul_fp_ceil(a: u128, b: u128) -> u128 {
    let fp = U256::from(FP);
    let q = (U256::from(a) * U256::from(b) + fp - U256::from(1u8)) / fp;
    q.try_into().unwrap_or(u128::MAX)
}

/// Ceil-approximation of ((p−1)/p)^e in FP fixed point, O(log e) squarings.
fn pow_decay_fp(mut e: u64, periods: u128) -> u128 {
    // ceil((p−1)·FP/p) via U256 — periods is unbounded above.
    let fp = U256::from(FP);
    let p = U256::from(periods);
    let mut base: u128 = ((U256::from(periods - 1) * fp + p - U256::from(1u8)) / p)
        .try_into()
        .unwrap_or(0);
    let mut result = FP;
    while e > 0 {
        if e & 1 == 1 {
            result = mul_fp_ceil(result, base);
        }
        base = mul_fp_ceil(base, base);
        e >>= 1;
    }
    result
}

/// The pot after e drip epochs: ⌈B·((P−1)/P)^e⌉ (release floors). Total
/// over all inputs: periods = 0 is treated as 1 (a horizon of one drip).
pub fn pot_after_release(b: u128, e: u64, periods: u128) -> u128 {
    if b == 0 || e == 0 {
        return b;
    }
    let pow = pow_decay_fp(e, periods.max(1));
    let fp = U256::from(FP);
    let q = (U256::from(b) * U256::from(pow) + fp - U256::from(1u8)) / fp;
    <U256 as TryInto<u128>>::try_into(q).unwrap_or(b).min(b)
}

impl Permafrost {
    // ── Storage ────────────────────────────────────────────────────

    fn underlying_pointer(&self) -> StoragePointer {
        StoragePointer::from_keyword("/underlying")
    }
    fn total_shares_pointer(&self) -> StoragePointer {
        StoragePointer::from_keyword("/total-shares")
    }
    fn liquidity_pointer(&self) -> StoragePointer {
        StoragePointer::from_keyword("/liquidity")
    }
    fn pot_pointer(&self) -> StoragePointer {
        StoragePointer::from_keyword("/pot")
    }
    fn pot_anchor_pointer(&self) -> StoragePointer {
        StoragePointer::from_keyword("/pot-anchor")
    }
    fn penalty_bps_pointer(&self) -> StoragePointer {
        StoragePointer::from_keyword("/penalty-bps")
    }
    fn name_pointer(&self) -> StoragePointer {
        StoragePointer::from_keyword("/name")
    }
    fn symbol_pointer(&self) -> StoragePointer {
        StoragePointer::from_keyword("/symbol")
    }
    fn drip_interval_pointer(&self) -> StoragePointer {
        StoragePointer::from_keyword("/drip-interval")
    }
    fn release_periods_pointer(&self) -> StoragePointer {
        StoragePointer::from_keyword("/release-periods")
    }

    fn underlying(&self) -> Result<AlkaneId> {
        let data = self.underlying_pointer().get();
        ensure!(!data.is_empty(), "not initialized");
        Ok(AlkaneId::try_from(data.as_slice().to_vec())?)
    }
    fn total_shares(&self) -> u128 {
        self.total_shares_pointer().get_value::<u128>()
    }
    fn set_total_shares(&self, value: u128) {
        self.total_shares_pointer().set_value::<u128>(value)
    }
    fn liquidity(&self) -> u128 {
        self.liquidity_pointer().get_value::<u128>()
    }
    fn set_liquidity(&self, value: u128) {
        self.liquidity_pointer().set_value::<u128>(value)
    }
    fn pot(&self) -> u128 {
        self.pot_pointer().get_value::<u128>()
    }
    fn set_pot(&self, value: u128) {
        self.pot_pointer().set_value::<u128>(value)
    }
    fn pot_anchor(&self) -> u64 {
        self.pot_anchor_pointer().get_value::<u64>()
    }
    fn set_pot_anchor(&self, value: u64) {
        self.pot_anchor_pointer().set_value::<u64>(value)
    }
    fn penalty_bps(&self) -> u128 {
        self.penalty_bps_pointer().get_value::<u128>()
    }
    /// Blocks per drip. `.max(1)` guards the pre-init zero — harmless
    /// (B is zero there too) but keeps view paths panic-free.
    fn drip_interval(&self) -> u64 {
        self.drip_interval_pointer().get_value::<u64>().max(1)
    }
    /// Drips to (asymptotically) empty the pot once. Same pre-init guard.
    fn release_periods(&self) -> u128 {
        self.release_periods_pointer().get_value::<u128>().max(1)
    }

    // ── Release (the drip) ────────────────────────────────────────

    /// Virtual release at height `h`: (B after, h₀ after). Read-only. While no
    /// one stands (S ≤ dead shares), epochs are consumed but nothing is
    /// released — the pot waits whole for the next generation.
    fn released_pot_at(&self, h: u64) -> (u128, u64) {
        let interval = self.drip_interval();
        let h0 = self.pot_anchor();
        let e = h.saturating_sub(h0) / interval;
        let b = if self.total_shares() > DEAD_SHARES {
            pot_after_release(self.pot(), e, self.release_periods())
        } else {
            self.pot()
        };
        (b, h0 + e * interval)
    }

    /// The first step of every mutating operation. The anchor advances even
    /// when the pot is empty or frozen, so a later tithe is never decayed
    /// by epochs that predate it, and the epochs of an empty vault are
    /// consumed without releasing to unburnable dead shares.
    fn release(&self) -> u128 {
        let (b, h0) = self.released_pot_at(self.height());
        self.set_pot(b);
        self.set_pot_anchor(h0);
        b
    }

    // ── Core math ─────────────────────────────────────────────────

    /// Shares minted for a deposit of `amount` at rate (L−B)/S. The
    /// genesis branch mints 1:1 and burns DEAD_SHARES; the regular branch
    /// floors — the depositor's rounding loss is strictly under one share's
    /// worth (P1).
    fn deposit_shares(&self, amount: u128, active: u128, s: u128) -> Result<u128> {
        if s == 0 {
            ensure!(
                amount > DEAD_SHARES,
                "first deposit must exceed {} units",
                DEAD_SHARES
            );
            Ok(amount - DEAD_SHARES)
        } else {
            let minted = muldiv_floor(amount, s, active)?;
            ensure!(minted > 0, "deposit too small: mints zero shares");
            Ok(minted)
        }
    }

    /// (payout, tithe, pot_after) for burning `b` shares at state (L=l,
    /// B=pot, S=s), post-release. W = ⌊b·(L−B)/S⌋; π = ⌈p·W⌉ into the pot.
    /// The rule is the same for everyone, including the very last exit —
    /// their tithe joins the dowry that waits for the next generation.
    fn exit_amounts(&self, b: u128, l: u128, pot: u128, s: u128) -> Result<(u128, u128, u128)> {
        ensure!(s > 0, "vault is empty");
        let active = l - pot;
        let w = muldiv_floor(b, active, s)?; // ≤ active since b ≤ s
        let tithe = muldiv_ceil(w, self.penalty_bps(), BPS)?;
        Ok((w - tithe, tithe, pot + tithe))
    }

    // ── Handlers ──────────────────────────────────────────────────

    fn initialize(
        &self,
        underlying: AlkaneId,
        penalty_bps: u128,
        name: u128,
        symbol: u128,
        drip_interval: u128,
        release_periods: u128,
    ) -> Result<CallResponse> {
        self.observe_initialization()
            .map_err(|_| anyhow!("already initialized"))?;
        let context = self.context()?;
        ensure!(penalty_bps <= BPS, "tithe rate exceeds 100%");
        ensure!(
            !(underlying.block == 0 && underlying.tx == 0),
            "underlying must be set"
        );
        ensure!(underlying != context.myself, "underlying cannot be the vault itself");
        ensure!(
            drip_interval >= 1 && drip_interval <= u64::MAX as u128,
            "drip_interval must be a positive block count"
        );
        ensure!(release_periods >= 1, "release_periods must be positive");
        self.underlying_pointer().set(Arc::new(underlying.into()));
        self.penalty_bps_pointer().set_value::<u128>(penalty_bps);
        self.name_pointer()
            .set(Arc::new(trim(name).into_bytes()));
        self.symbol_pointer()
            .set(Arc::new(trim(symbol).into_bytes()));
        self.drip_interval_pointer()
            .set_value::<u64>(drip_interval as u64);
        self.release_periods_pointer()
            .set_value::<u128>(release_periods);
        self.set_pot_anchor(self.height());
        Ok(CallResponse::forward(&context.incoming_alkanes))
    }

    /// Pull the underlying transfer out of the incoming parcel. Returns the
    /// amount and a response pre-filled with every OTHER incoming token
    /// refunded (nothing but the underlying is ever consumed) and
    /// data = amount (u128 LE); callers may extend or overwrite both.
    fn take_underlying(&self, context: &Context) -> Result<(u128, CallResponse)> {
        let underlying = self.underlying()?;
        let amount = context
            .incoming_alkanes
            .0
            .iter()
            .find(|t| t.id == underlying)
            .map(|t| t.value)
            .ok_or_else(|| anyhow!("no underlying tokens in incoming transaction"))?;
        ensure!(amount > 0, "underlying transfer is zero");
        let mut response = CallResponse::default();
        for t in context.incoming_alkanes.0.iter() {
            if t.id != underlying {
                response.alkanes.0.push(t.clone());
            }
        }
        response.data = amount.to_le_bytes().to_vec();
        Ok((amount, response))
    }

    fn deposit(&self) -> Result<CallResponse> {
        let context = self.context()?;
        let (amount, mut response) = self.take_underlying(&context)?;

        let pot = self.release();
        let s = self.total_shares();
        let l = self.liquidity();
        let minted = self.deposit_shares(amount, l - pot, s)?;

        // Genesis credits the full amount to S (user shares + dead shares).
        let supply_delta = if s == 0 { amount } else { minted };
        self.set_total_shares(
            s.checked_add(supply_delta)
                .ok_or_else(|| anyhow!("share supply overflow"))?,
        );
        self.set_liquidity(
            l.checked_add(amount)
                .ok_or_else(|| anyhow!("liquidity overflow"))?,
        );

        response.alkanes.0.push(AlkaneTransfer {
            id: context.myself,
            value: minted,
        });
        response.data = minted.to_le_bytes().to_vec();
        Ok(response)
    }

    fn withdraw(&self) -> Result<CallResponse> {
        let context = self.context()?;
        let b = context
            .incoming_alkanes
            .0
            .iter()
            .find(|t| t.id == context.myself)
            .map(|t| t.value)
            .ok_or_else(|| anyhow!("no shares in incoming transaction"))?;
        ensure!(b > 0, "cannot withdraw zero shares");

        let pot = self.release();
        let s = self.total_shares();
        let l = self.liquidity();
        // Dead shares never circulate, so b ≤ S − DEAD_SHARES structurally;
        // defend anyway — an inflated b would drain the vault.
        ensure!(
            b.checked_add(DEAD_SHARES).is_some_and(|t| t <= s),
            "share amount exceeds circulating supply"
        );

        let (payout, tithe, pot_after) = self.exit_amounts(b, l, pot, s)?;

        self.set_total_shares(s - b);
        self.set_liquidity(l - payout);
        self.set_pot(pot_after);

        let mut response = CallResponse::default();
        // Refund anything that is not the shares being burned.
        for t in context.incoming_alkanes.0.iter() {
            if t.id != context.myself {
                response.alkanes.0.push(t.clone());
            }
        }
        if payout > 0 {
            response.alkanes.0.push(AlkaneTransfer {
                id: self.underlying()?,
                value: payout,
            });
        }
        let mut data = payout.to_le_bytes().to_vec();
        data.extend_from_slice(&tithe.to_le_bytes());
        response.data = data;
        Ok(response)
    }

    fn donate_pot(&self) -> Result<CallResponse> {
        let context = self.context()?;
        let (amount, response) = self.take_underlying(&context)?;

        // If no one stands yet, the release is frozen and the tribute will
        // sit whole in the pot until the vault has standers to drip to.
        let pot = self.release();

        self.set_liquidity(
            self.liquidity()
                .checked_add(amount)
                .ok_or_else(|| anyhow!("liquidity overflow"))?,
        );
        self.set_pot(
            pot.checked_add(amount)
                .ok_or_else(|| anyhow!("pot overflow"))?,
        );
        Ok(response)
    }

    fn donate_boost(&self) -> Result<CallResponse> {
        let context = self.context()?;
        let (amount, response) = self.take_underlying(&context)?;
        self.release();

        // A boost pays those standing NOW. Before genesis it would be
        // swallowed whole by the first depositor; in a dead vault it would
        // lock forever into unburnable shares. Revert refunds the donor.
        ensure!(
            self.total_shares() > DEAD_SHARES,
            "no one standing to receive the boost"
        );

        self.set_liquidity(
            self.liquidity()
                .checked_add(amount)
                .ok_or_else(|| anyhow!("liquidity overflow"))?,
        );
        Ok(response)
    }

    // ── Views ─────────────────────────────────────────────────────

    fn get_name(&self) -> Result<CallResponse> {
        let context = self.context()?;
        let mut response = CallResponse::forward(&context.incoming_alkanes);
        response.data = self.name_pointer().get().as_ref().clone();
        Ok(response)
    }

    fn get_symbol(&self) -> Result<CallResponse> {
        let context = self.context()?;
        let mut response = CallResponse::forward(&context.incoming_alkanes);
        response.data = self.symbol_pointer().get().as_ref().clone();
        Ok(response)
    }

    fn get_total_shares(&self) -> Result<CallResponse> {
        let context = self.context()?;
        let mut response = CallResponse::forward(&context.incoming_alkanes);
        response.data = self.total_shares().to_le_bytes().to_vec();
        Ok(response)
    }

    fn get_vault_balance(&self) -> Result<CallResponse> {
        let context = self.context()?;
        let mut response = CallResponse::forward(&context.incoming_alkanes);
        response.data = self.liquidity().to_le_bytes().to_vec();
        Ok(response)
    }

    fn get_penalty_bps(&self) -> Result<CallResponse> {
        let context = self.context()?;
        let mut response = CallResponse::forward(&context.incoming_alkanes);
        response.data = self.penalty_bps().to_le_bytes().to_vec();
        Ok(response)
    }

    fn get_underlying(&self) -> Result<CallResponse> {
        let context = self.context()?;
        let underlying = self.underlying()?;
        let mut response = CallResponse::forward(&context.incoming_alkanes);
        response.data = underlying.block.to_le_bytes().to_vec();
        response.data.extend_from_slice(&underlying.tx.to_le_bytes());
        Ok(response)
    }

    fn get_pot(&self) -> Result<CallResponse> {
        let context = self.context()?;
        let mut response = CallResponse::forward(&context.incoming_alkanes);
        response.data = self.pot().to_le_bytes().to_vec();
        Ok(response)
    }

    /// Raw (L, B, S) — pure storage read, never reverts (returns zeros
    /// before initialization). Pending drips are NOT applied; clients can
    /// project them from h₀ (opcode 109).
    fn get_state(&self) -> Result<CallResponse> {
        let context = self.context()?;
        let mut response = CallResponse::forward(&context.incoming_alkanes);
        response.data = self.liquidity().to_le_bytes().to_vec();
        response.data.extend_from_slice(&self.pot().to_le_bytes());
        response.data.extend_from_slice(&self.total_shares().to_le_bytes());
        Ok(response)
    }

    fn get_pot_anchor(&self) -> Result<CallResponse> {
        let context = self.context()?;
        let mut response = CallResponse::forward(&context.incoming_alkanes);
        response.data = (self.pot_anchor() as u128).to_le_bytes().to_vec();
        Ok(response)
    }

    fn get_drip_interval(&self) -> Result<CallResponse> {
        let context = self.context()?;
        let mut response = CallResponse::forward(&context.incoming_alkanes);
        response.data = (self.drip_interval() as u128).to_le_bytes().to_vec();
        Ok(response)
    }

    fn get_release_periods(&self) -> Result<CallResponse> {
        let context = self.context()?;
        let mut response = CallResponse::forward(&context.incoming_alkanes);
        response.data = self.release_periods().to_le_bytes().to_vec();
        Ok(response)
    }

    /// The logo lives in the bytecode itself, so every clone created from
    /// this WASM serves the identical image — immutable by construction.
    fn get_data(&self) -> Result<CallResponse> {
        let context = self.context()?;
        let mut response = CallResponse::forward(&context.incoming_alkanes);
        response.data = include_bytes!("./brick-firn.svg").to_vec();
        Ok(response)
    }

    fn quote_deposit(&self, amount: u128) -> Result<CallResponse> {
        let context = self.context()?;
        let (pot, _) = self.released_pot_at(self.height());
        let minted = self.deposit_shares(amount, self.liquidity() - pot, self.total_shares())?;
        let mut response = CallResponse::forward(&context.incoming_alkanes);
        response.data = minted.to_le_bytes().to_vec();
        Ok(response)
    }

    fn quote_withdraw(&self, shares: u128) -> Result<CallResponse> {
        let context = self.context()?;
        let s = self.total_shares();
        ensure!(
            shares > 0 && shares.checked_add(DEAD_SHARES).is_some_and(|t| t <= s),
            "invalid share amount"
        );
        let (pot, _) = self.released_pot_at(self.height());
        let (payout, tithe, _) = self.exit_amounts(shares, self.liquidity(), pot, s)?;
        let mut response = CallResponse::forward(&context.incoming_alkanes);
        response.data = payout.to_le_bytes().to_vec();
        response.data.extend_from_slice(&tithe.to_le_bytes());
        Ok(response)
    }
}

// Not compiled into the test binary: tests exercise the vendored WASM inside
// the metashrew VM, and the no_mangle __execute export would otherwise drag
// unresolvable "env" host imports into the wasm-bindgen test runner.
#[cfg(not(test))]
declare_alkane! {
    impl AlkaneResponder for Permafrost {
        type Message = PermafrostMessage;
    }
}

#[cfg(test)]
mod tests;
