//! # PERMAFROST integration: perpetual vault (a tontine)
//!
//! A `Mirror` struct replicates the contract's integer state machine
//! (including the fixed-point drip) so every on-chain state is asserted
//! against an independently-computed expectation, exactly.
//!
//! Test 1 (`permafrost_neutrality_and_flat_price`): within one drip epoch —
//! genesis burns dead shares (P9), deposits are neutral (P1), exits are
//! neutral (P5: the tithe goes into the pot, the rate does not move),
//! conservation L′ + payout = L (P3), and the flat-price law (L2): two
//! half-slices pay exactly the same tithe as one lump.
//!
//! Test 2 (`permafrost_drip_and_last_exit`): the drip — one epoch releases
//! exactly ⌈B·364/365⌉ (ideal bound ±1 dust unit), 365 epochs release
//! 63–64% (→ e⁻¹), the rate only rises (P4); release ordering on both
//! doors (exiters take the drip they stood for, depositors mint after it);
//! the last exit pays the flat tithe like everyone, the pot survives the
//! exodus as a dowry, frozen while no one stands and thawed by the next
//! generation.
//!
//! Test 3 (`permafrost_donate_and_eternity`): the two tribute roads — a
//! pre-genesis pot-donation is accepted and waits frozen; genesis mints
//! 1:1 over the dowry; a live pot-donation is rate-neutral and drips like
//! tithes; a boost is instant and up-only (and reverts with a refund when
//! no one stands); the fixed-point power is pinned across e up to
//! u64::MAX.
//!
//! Test 4 (`permafrost_release_fuel`): fuel of a full Withdraw across
//! quiet-period magnitudes up to the u64 height ceiling.

use super::helpers::{
    clear_test_environment, create_block_with_coinbase_tx,
    create_multiple_cellpack_with_witness_and_in,
    create_multiple_cellpack_with_witness_and_txins_edicts, deploy_mock_token, get_address,
    get_permafrost_wasm_bytes, get_sheet_by_outpoint, index_block,
    init_with_multiple_cellpacks_with_tx, simulate_cellpack, simulate_parcel, AtomicPointer,
    BalanceSheet, MessageContextParcel, Protostone, ProtostoneEdict, Protostones, Runestone,
    ADDRESS1,
};
use super::test_log;
use alkanes_support::cellpack::Cellpack;
use alkanes_support::id::AlkaneId;
use anyhow::Result;
use bitcoin::address::NetworkChecked;
use bitcoin::{Address, Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness};
use protorune_support::balance_sheet::ProtoruneRuneId;
use protorune_support::rune_transfer::RuneTransfer;
use wasm_bindgen_test::wasm_bindgen_test;

const MOCK_UNDERLYING_TX: u128 = 0xa10;
const VAULT_SLOT: u128 = 0xf057;
const PENALTY_BPS: u128 = 1000; // the reference 10% tithe
const BPS: u128 = 10_000;
const DEAD_SHARES: u128 = 1000;
const DRIP_INTERVAL: u64 = 144;
const YEAR_DAYS: u128 = 365;
const FP: u128 = 1 << 64;

fn underlying_id() -> AlkaneId {
    AlkaneId { block: 4, tx: MOCK_UNDERLYING_TX }
}
fn vault_id() -> AlkaneId {
    AlkaneId { block: 4, tx: VAULT_SLOT }
}
fn rid(id: &AlkaneId) -> ProtoruneRuneId {
    ProtoruneRuneId { block: id.block, tx: id.tx }
}

// ── Opcodes ──────────────────────────────────────────────────────
const OP_INIT: u128 = 0;
const OP_DEPOSIT: u128 = 1;
const OP_WITHDRAW: u128 = 2;
const OP_DONATE_POT: u128 = 3;
const OP_DONATE_BOOST: u128 = 4;
const OP_PENALTY_BPS: u128 = 103;
const OP_QUOTE_DEPOSIT: u128 = 105;
const OP_QUOTE_WITHDRAW: u128 = 106;
const OP_GET_POT: u128 = 107;
const OP_GET_STATE: u128 = 108;
const OP_GET_POT_ANCHOR: u128 = 109;

// ── Mirror: the contract's integer state machine, replicated ─────

fn mul_fp_ceil(a: u128, b: u128) -> u128 {
    (a.checked_mul(b).expect("fp mul overflow") + FP - 1) / FP
}

fn pow_decay_fp(mut e: u64, periods: u128) -> u128 {
    let mut base = ((periods - 1) * FP + (periods - 1)) / periods;
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

fn pot_after_release(b: u128, e: u64, periods: u128) -> u128 {
    if b == 0 || e == 0 {
        return b;
    }
    let pow = pow_decay_fp(e, periods);
    ((b.checked_mul(pow).expect("pot mul overflow") + FP - 1) / FP).min(b)
}

/// Off-chain replica of the vault: L, B, S, h₀ with the same rounding
/// discipline. Amounts in these tests stay ≤ ~2e11 so plain u128 suffices.
struct Mirror {
    l: u128,
    b: u128,
    s: u128,
    h0: u64,
}

impl Mirror {
    fn new(anchor: u64) -> Self {
        Mirror { l: 0, b: 0, s: 0, h0: anchor }
    }
    fn active(&self) -> u128 {
        self.l - self.b
    }
    fn release(&mut self, h: u64) {
        let e = h.saturating_sub(self.h0) / DRIP_INTERVAL;
        // While no one stands, the pot is frozen: epochs are consumed
        // (anchor advances) but nothing is released.
        if self.s > DEAD_SHARES {
            self.b = pot_after_release(self.b, e, YEAR_DAYS);
        }
        self.h0 += e * DRIP_INTERVAL;
    }
    fn deposit(&mut self, h: u64, d: u128) -> u128 {
        self.release(h);
        let (minted, delta) = if self.s == 0 {
            (d - DEAD_SHARES, d)
        } else {
            let m = d * self.s / self.active();
            (m, m)
        };
        self.s += delta;
        self.l += d;
        minted
    }
    fn withdraw(&mut self, h: u64, shares: u128) -> (u128, u128) {
        self.release(h);
        let w = shares * self.active() / self.s;
        let tithe = (w * PENALTY_BPS + (BPS - 1)) / BPS;
        let payout = w - tithe;
        self.s -= shares;
        self.l -= payout;
        self.b += tithe;
        (payout, tithe)
    }
    fn donate_pot(&mut self, h: u64, d: u128) {
        self.release(h);
        self.l += d;
        self.b += d;
    }
    fn donate_boost(&mut self, h: u64, d: u128) {
        self.release(h);
        self.l += d;
    }
}

// ── Harness plumbing ─────────────────────────────────────────────

fn call_with_inputs(
    height: u32,
    target: AlkaneId,
    inputs: Vec<u128>,
    input_outpoints: Vec<OutPoint>,
) -> Result<OutPoint> {
    let mut block = create_block_with_coinbase_tx(height);
    let cellpack = Cellpack { target, inputs };
    let txins = input_outpoints
        .into_iter()
        .map(|previous_output| TxIn {
            previous_output,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        })
        .collect();
    let tx =
        create_multiple_cellpack_with_witness_and_txins_edicts(vec![cellpack], txins, false, vec![]);
    let txid = tx.compute_txid();
    block.txdata.push(tx);
    index_block(&block, height)?;
    Ok(OutPoint { txid, vout: 0 })
}

fn call_with_input(
    height: u32,
    target: AlkaneId,
    inputs: Vec<u128>,
    input_outpoint: OutPoint,
) -> Result<OutPoint> {
    let mut block = create_block_with_coinbase_tx(height);
    let cellpack = Cellpack { target, inputs };
    let tx = create_multiple_cellpack_with_witness_and_in(
        Witness::new(),
        vec![cellpack],
        input_outpoint,
        false,
    );
    let txid = tx.compute_txid();
    block.txdata.push(tx);
    index_block(&block, height)?;
    Ok(OutPoint { txid, vout: 0 })
}

/// Edict-only token separation (no protomessage): route exact amounts to
/// real outputs. Edicts must cover the input in full — the remainder joins
/// output 0 via the pointer.
fn separate(
    height: u32,
    input_outpoint: OutPoint,
    edicts: Vec<ProtostoneEdict>,
    n_outputs: u32,
) -> Result<bitcoin::Txid> {
    let protostone = Protostone {
        message: vec![],
        pointer: Some(0),
        refund: Some(0),
        edicts,
        from: None,
        burn: None,
        protocol_tag: 1,
    };
    let runestone_script: ScriptBuf = (Runestone {
        etching: None,
        pointer: Some(0),
        edicts: Vec::new(),
        mint: None,
        protocol: vec![protostone].encipher().ok(),
    })
    .encipher();

    let address: Address<NetworkChecked> = get_address(&ADDRESS1().as_str());
    let mut output: Vec<TxOut> = (0..n_outputs)
        .map(|_| TxOut {
            value: Amount::from_sat(100_000_000),
            script_pubkey: address.script_pubkey(),
        })
        .collect();
    output.push(TxOut {
        value: Amount::from_sat(0),
        script_pubkey: runestone_script,
    });

    let tx = Transaction {
        version: bitcoin::blockdata::transaction::Version::ONE,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: input_outpoint,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output,
    };
    let txid = tx.compute_txid();
    let mut block = create_block_with_coinbase_tx(height);
    block.txdata.push(tx);
    index_block(&block, height)?;
    Ok(txid)
}

fn view_u128(height: u32, opcode: u128) -> Result<u128> {
    view_u128_args(height, vec![opcode])
}

fn view_u128_args(height: u32, inputs: Vec<u128>) -> Result<u128> {
    let (resp, _) = simulate_cellpack(
        height as u64,
        Cellpack { target: vault_id(), inputs },
    )?;
    anyhow::ensure!(resp.data.len() >= 16, "view returned {} bytes", resp.data.len());
    let mut b = [0u8; 16];
    b.copy_from_slice(&resp.data[0..16]);
    Ok(u128::from_le_bytes(b))
}

fn view_u128_pair(height: u32, inputs: Vec<u128>) -> Result<(u128, u128)> {
    let (resp, _) = simulate_cellpack(
        height as u64,
        Cellpack { target: vault_id(), inputs },
    )?;
    anyhow::ensure!(resp.data.len() >= 32, "view returned {} bytes", resp.data.len());
    let mut a = [0u8; 16];
    let mut b = [0u8; 16];
    a.copy_from_slice(&resp.data[0..16]);
    b.copy_from_slice(&resp.data[16..32]);
    Ok((u128::from_le_bytes(a), u128::from_le_bytes(b)))
}

/// GetState (opcode 108): raw (L, B, S).
fn view_state(height: u32) -> Result<(u128, u128, u128)> {
    let (resp, _) = simulate_cellpack(
        height as u64,
        Cellpack { target: vault_id(), inputs: vec![OP_GET_STATE] },
    )?;
    anyhow::ensure!(resp.data.len() >= 48, "state view returned {} bytes", resp.data.len());
    let word = |i: usize| {
        let mut w = [0u8; 16];
        w.copy_from_slice(&resp.data[i * 16..(i + 1) * 16]);
        u128::from_le_bytes(w)
    };
    Ok((word(0), word(1), word(2)))
}

fn assert_state(tag: &str, height: u32, m: &Mirror) -> Result<()> {
    let (l, b, s) = view_state(height)?;
    assert_eq!((l, b, s), (m.l, m.b, m.s), "{}: on-chain state != mirror", tag);
    Ok(())
}

/// Pack a string into a u128 (LE bytes) — the standard alkanes token-name
/// encoding the contract decodes with `trim`.
fn pack_str(s: &str) -> u128 {
    let bytes = s.as_bytes();
    assert!(bytes.len() <= 16, "packed strings fit 16 bytes");
    let mut b = [0u8; 16];
    b[..bytes.len()].copy_from_slice(bytes);
    u128::from_le_bytes(b)
}

fn view_string(height: u32, opcode: u128) -> Result<String> {
    let (resp, _) = simulate_cellpack(
        height as u64,
        Cellpack { target: vault_id(), inputs: vec![opcode] },
    )?;
    Ok(String::from_utf8_lossy(&resp.data).to_string())
}

const OP_GET_NAME: u128 = 99;
const OP_GET_SYMBOL: u128 = 100;
const VAULT_NAME: &str = "FROST-BTCUSD";
const VAULT_SYMBOL: &str = "fBTCUSD";

/// Deploy the mock underlying (mint `lp_units`) and the vault (deploy + Initialize
/// in one tx, with the name/symbol passed as init args — nothing is
/// hardcoded in the contract). Returns (und_outpoint, next_height); the
/// drip anchor is `next_height - 1` (the Initialize block).
fn setup_vault(start_height: u32, lp_units: u128) -> Result<(OutPoint, u32)> {
    let mut h = start_height;
    let und_out = deploy_mock_token(h, MOCK_UNDERLYING_TX, lp_units)?;
    h += 1;

    let block = init_with_multiple_cellpacks_with_tx(
        vec![get_permafrost_wasm_bytes().to_vec()],
        vec![Cellpack {
            target: AlkaneId { block: 3, tx: VAULT_SLOT },
            inputs: vec![
                OP_INIT,
                underlying_id().block,
                underlying_id().tx,
                PENALTY_BPS,
                pack_str(VAULT_NAME),
                pack_str(VAULT_SYMBOL),
                DRIP_INTERVAL as u128,
                YEAR_DAYS,
            ],
        }],
    );
    index_block(&block, h)?;
    h += 1;

    assert_eq!(view_u128(h, OP_PENALTY_BPS)?, PENALTY_BPS, "penalty bps view");
    assert_eq!(view_string(h, OP_GET_NAME)?, VAULT_NAME, "name set at init, not hardcoded");
    assert_eq!(view_string(h, OP_GET_SYMBOL)?, VAULT_SYMBOL, "symbol set at init");
    assert_eq!(view_u128(h, 110)?, DRIP_INTERVAL as u128, "drip interval set at init");
    assert_eq!(view_u128(h, 111)?, YEAR_DAYS, "release periods set at init");
    let (logo, _) = simulate_cellpack(
        h as u64,
        Cellpack { target: vault_id(), inputs: vec![1000] },
    )?;
    assert_eq!(
        logo.data,
        include_bytes!("../brick-firn.svg").to_vec(),
        "opcode 1000 must serve the embedded logo byte-for-byte"
    );
    Ok((und_out, h))
}

#[wasm_bindgen_test]
fn permafrost_neutrality_and_flat_price() -> Result<()> {
    clear_test_environment();
    test_log!("\n=== PERMAFROST: neutrality of deposits AND exits, flat price ===");

    let mut h: u32 = 890_000;
    // 100 + 36.5 + 26 = 162.5 LP-units of 8-decimal scale (chosen so every
    // expected value below is exact).
    let (und_out, nh) = setup_vault(h, 162_500_000_000)?;
    h = nh;
    let mut m = Mirror::new((h - 1) as u64);

    let split = separate(
        h,
        und_out,
        vec![
            ProtostoneEdict { id: rid(&underlying_id()), amount: 100_000_000_000, output: 0 },
            ProtostoneEdict { id: rid(&underlying_id()), amount: 36_500_000_000, output: 1 },
            ProtostoneEdict { id: rid(&underlying_id()), amount: 26_000_000_000, output: 2 },
        ],
        3,
    )?;
    h += 1;

    // ── Genesis: 100e9 LP → 100e9 − DEAD_SHARES to user (P9) ─────
    let d1 = call_with_input(h, vault_id(), vec![OP_DEPOSIT], OutPoint { txid: split, vout: 0 })?;
    let minted = m.deposit(h as u64, 100_000_000_000);
    h += 1;
    assert_eq!(minted, 100_000_000_000 - DEAD_SHARES);
    assert_eq!(get_sheet_by_outpoint(&d1)?.get_cached(&rid(&vault_id())), minted);
    assert_state("genesis", h, &m)?;

    // ── Deposit at rate 1: neutral (P1) ──────────────────────────
    let d2 = call_with_input(h, vault_id(), vec![OP_DEPOSIT], OutPoint { txid: split, vout: 1 })?;
    let minted = m.deposit(h as u64, 36_500_000_000);
    h += 1;
    assert_eq!(minted, 36_500_000_000, "1:1 mint at rate 1");
    assert_state("deposit", h, &m)?;

    // ── Exit: the tithe goes into the pot, NOT the rate (P5) ─────
    // W = 36.5e9, π = 3.65e9, payout = 32.85e9; rate (L−B)/S stays exactly 1.
    let (active_before, s_before) = (m.active(), m.s);
    let w1 = call_with_input(h, vault_id(), vec![OP_WITHDRAW], d2)?;
    let (payout, tithe) = m.withdraw(h as u64, 36_500_000_000);
    h += 1;
    assert_eq!((payout, tithe), (32_850_000_000, 3_650_000_000));
    assert_eq!(get_sheet_by_outpoint(&w1)?.get_cached(&rid(&underlying_id())), payout);
    assert_state("exit", h, &m)?;
    assert_eq!((m.l, m.b, m.s), (103_650_000_000, 3_650_000_000, 100_000_000_000));
    // Exit neutrality, exactly: active/S unchanged (cross-multiplied).
    assert_eq!(
        m.active() * s_before,
        active_before * m.s,
        "someone's exit must not move the rate at all"
    );

    // ── Deposit after a tithe: still mints at rate 1 (pot excluded) ──
    let d3 = call_with_input(h, vault_id(), vec![OP_DEPOSIT], OutPoint { txid: split, vout: 2 })?;
    let minted = m.deposit(h as u64, 26_000_000_000);
    h += 1;
    assert_eq!(minted, 26_000_000_000, "pot is excluded from the mint rate");
    assert_state("deposit2", h, &m)?;

    // ── Flat price law (L2): lump quote == sum of slices ─────────
    let (q_payout, q_tithe) = view_u128_pair(h, vec![OP_QUOTE_WITHDRAW, 26_000_000_000])?;
    assert_eq!((q_payout, q_tithe), (23_400_000_000, 2_600_000_000), "lump quote");

    // Split the 26e9 shares into two 13e9 slices and exit both.
    let shares_split = separate(
        h,
        d3,
        vec![
            ProtostoneEdict { id: rid(&vault_id()), amount: 13_000_000_000, output: 0 },
            ProtostoneEdict { id: rid(&vault_id()), amount: 13_000_000_000, output: 1 },
        ],
        2,
    )?;
    h += 1;

    let mut sliced_payout = 0u128;
    let mut sliced_tithe = 0u128;
    for vout in 0..2 {
        let w = call_with_input(
            h, vault_id(), vec![OP_WITHDRAW],
            OutPoint { txid: shares_split, vout },
        )?;
        let (p, t) = m.withdraw(h as u64, 13_000_000_000);
        h += 1;
        assert_eq!(get_sheet_by_outpoint(&w)?.get_cached(&rid(&underlying_id())), p);
        sliced_payout += p;
        sliced_tithe += t;
    }
    assert_eq!(
        (sliced_payout, sliced_tithe),
        (q_payout, q_tithe),
        "slicing buys nothing: two halves pay exactly the lump tithe"
    );
    assert_state("sliced", h, &m)?;
    assert_eq!((m.l, m.b, m.s), (106_250_000_000, 6_250_000_000, 100_000_000_000));

    test_log!("[PERMAFROST] neutrality+flat-price OK: rate pinned at 1.0 throughout, pot = 6.25e9");
    Ok(())
}

#[wasm_bindgen_test]
fn permafrost_drip_and_last_exit() -> Result<()> {
    clear_test_environment();
    test_log!("\n=== PERMAFROST: the drip, e^-1 over a year, exodus and dowry ===");

    let mut h: u32 = 891_000;
    // 100 + 36.5 + 10 LP for the vault + 0.01 + 0.01 kept for the revival
    // and the resume-trigger.
    let (und_out, nh) = setup_vault(h, 146_520_000_000)?;
    h = nh;
    let anchor = (h - 1) as u64;
    let mut m = Mirror::new(anchor);

    let split = separate(
        h,
        und_out,
        vec![
            ProtostoneEdict { id: rid(&underlying_id()), amount: 100_000_000_000, output: 0 },
            ProtostoneEdict { id: rid(&underlying_id()), amount: 36_500_000_000, output: 1 },
            ProtostoneEdict { id: rid(&underlying_id()), amount: 10_000_000_000, output: 2 },
            ProtostoneEdict { id: rid(&underlying_id()), amount: 10_000_000, output: 3 },
            ProtostoneEdict { id: rid(&underlying_id()), amount: 10_000_000, output: 4 },
        ],
        5,
    )?;
    h += 1;

    // Fill the pot: genesis 100e9, deposit 36.5e9, exit 36.5e9 → B = 3.65e9.
    let d1 = call_with_input(h, vault_id(), vec![OP_DEPOSIT], OutPoint { txid: split, vout: 0 })?;
    m.deposit(h as u64, 100_000_000_000);
    h += 1;
    let d2 = call_with_input(h, vault_id(), vec![OP_DEPOSIT], OutPoint { txid: split, vout: 1 })?;
    m.deposit(h as u64, 36_500_000_000);
    h += 1;
    call_with_input(h, vault_id(), vec![OP_WITHDRAW], d2)?;
    m.withdraw(h as u64, 36_500_000_000);
    h += 1;
    assert_state("pot filled", h, &m)?;
    assert_eq!(m.b, 3_650_000_000);

    // ── Release ordering, explicitly: the drip is credited BEFORE the
    // caller's own math, on both doors ───────────────────────────
    // Same state, two heights: one block before the epoch boundary (e=0)
    // vs at the boundary (e=1). An exiter's payout must GROW — their own
    // call releases the drip they stood for, and W is computed after it.
    // A depositor's mint must SHRINK — the release lands first, so the
    // pending drip belongs to the incumbents and is not captured by the
    // newcomer.
    let h_eve = (anchor + DRIP_INTERVAL - 1) as u32;
    let h_dawn = (anchor + DRIP_INTERVAL) as u32;
    let (pay_eve, _) = view_u128_pair(h_eve, vec![OP_QUOTE_WITHDRAW, 50_000_000_000])?;
    let (pay_dawn, _) = view_u128_pair(h_dawn, vec![OP_QUOTE_WITHDRAW, 50_000_000_000])?;
    assert_eq!(pay_eve, 45_000_000_000, "flat rate before the boundary");
    assert!(
        pay_dawn > pay_eve,
        "an exit at the boundary must include the freshly released drip: {} vs {}",
        pay_dawn, pay_eve
    );
    let mint_eve = view_u128_args(h_eve, vec![OP_QUOTE_DEPOSIT, 10_000_000_000])?;
    let mint_dawn = view_u128_args(h_dawn, vec![OP_QUOTE_DEPOSIT, 10_000_000_000])?;
    assert_eq!(mint_eve, 10_000_000_000, "1:1 mint before the boundary");
    assert!(
        mint_dawn < mint_eve,
        "a deposit at the boundary must mint at the post-release rate: {} vs {}",
        mint_dawn, mint_eve
    );

    // ── One epoch: the pot releases exactly 1/365 (±1 unit of dust) ──
    // Ideal: B·364/365 = 3.64e9. The ceil-approximated fixpoint power may
    // keep at most one extra unit in the pot — never less.
    let h_drip1 = (anchor + DRIP_INTERVAL) as u32;
    let d3 = call_with_input(
        h_drip1, vault_id(), vec![OP_DEPOSIT],
        OutPoint { txid: split, vout: 2 },
    )?;
    let d3_minted = m.deposit(h_drip1 as u64, 10_000_000_000);
    let h2 = h_drip1 + 1;
    assert_state("after 1 epoch", h2, &m)?;
    assert!(
        m.b == 3_640_000_000 || m.b == 3_640_000_001,
        "one epoch must release B/365 up to one dust unit: pot = {}",
        m.b
    );
    assert_eq!(view_u128(h2, OP_GET_POT_ANCHOR)?, (anchor + DRIP_INTERVAL) as u128);
    // The drip went into the rate: the deposit minted fewer shares than LP.
    assert!(d3_minted < 10_000_000_000, "rate must exceed 1 after the drip");
    assert_eq!(get_sheet_by_outpoint(&d3)?.get_cached(&rid(&vault_id())), d3_minted);

    // ── 365 epochs: what remains ≈ (364/365)^365 → e⁻¹ of the pot ──
    let pot_before_year = m.b;
    let year_remains = pot_after_release(pot_before_year, 365, YEAR_DAYS);
    assert!(
        year_remains * 100 > pot_before_year * 36 && year_remains * 100 < pot_before_year * 37,
        "a year of drips must leave ~36.7% (e^-1) of the pot, left {} of {}",
        year_remains, pot_before_year
    );

    let h_year = (anchor + DRIP_INTERVAL + DRIP_INTERVAL * 365) as u32;
    // Trigger the release with a 5e9-share exit (shares from d3).
    let year_split = separate(
        h_year - 1,
        d3,
        vec![
            ProtostoneEdict { id: rid(&vault_id()), amount: 5_000_000_000, output: 0 },
            ProtostoneEdict { id: rid(&vault_id()), amount: d3_minted - 5_000_000_000, output: 1 },
        ],
        2,
    )?;
    let w = call_with_input(
        h_year, vault_id(), vec![OP_WITHDRAW],
        OutPoint { txid: year_split, vout: 0 },
    )?;
    let (payout, tithe) = m.withdraw(h_year as u64, 5_000_000_000);
    let h3 = h_year + 1;
    assert_eq!(get_sheet_by_outpoint(&w)?.get_cached(&rid(&underlying_id())), payout);
    assert!(tithe > 0);
    assert_state("after a year", h3, &m)?;

    // ── Exodus: the last one pays the flat tithe like everyone ───
    // No privileges, no carve-out: their tithe joins the pot, and the pot
    // survives the exodus as a dowry for the next generation.
    let last_shares = m.s - DEAD_SHARES;
    let pot_before_exodus = m.b;
    let wl = call_with_inputs(
        h3, vault_id(), vec![OP_WITHDRAW],
        vec![d1, OutPoint { txid: year_split, vout: 1 }],
    )?;
    let (payout, tithe) = m.withdraw(h3 as u64, last_shares);
    let h4 = h3 + 1;
    assert!(tithe > 0, "the last exit pays the same flat tithe as any other");
    assert_eq!(get_sheet_by_outpoint(&wl)?.get_cached(&rid(&underlying_id())), payout);
    assert_state("exodus", h4, &m)?;
    assert_eq!(m.s, DEAD_SHARES, "only dead shares remain — S = 0 unreachable (P9)");
    let dowry = m.b;
    assert_eq!(dowry, pot_before_exodus + tithe, "the pot survives the exodus in full");
    assert_eq!(view_u128(h4, OP_GET_POT)?, dowry);

    // ── Emptiness freezes the drip: the dowry waits whole ────────
    // Ten epochs pass with no one standing. The revival deposit's own
    // release consumes them (the anchor advances) without releasing a
    // single unit — a drip would only feed unburnable dead shares.
    let exodus_anchor = m.h0;
    let h_revival = (exodus_anchor + DRIP_INTERVAL * 10) as u32;
    let d_rev = call_with_input(
        h_revival, vault_id(), vec![OP_DEPOSIT],
        OutPoint { txid: split, vout: 3 },
    )?;
    let minted = m.deposit(h_revival as u64, 10_000_000);
    let h5 = h_revival + 1;
    assert!(minted > 0);
    assert_eq!(get_sheet_by_outpoint(&d_rev)?.get_cached(&rid(&vault_id())), minted);
    assert_state("revival", h5, &m)?;
    assert_eq!(m.b, dowry, "not a unit may drip while no one stands");
    assert_eq!(
        view_u128(h5, OP_GET_POT_ANCHOR)?,
        (exodus_anchor + DRIP_INTERVAL * 10) as u128,
        "the empty epochs are consumed, not deferred"
    );

    // ── Drips resume for the new generation ──────────────────────
    // One epoch after the revival, the dowry starts paying the newcomer.
    let h_resume = (exodus_anchor + DRIP_INTERVAL * 11) as u32;
    call_with_input(h_resume, vault_id(), vec![OP_DEPOSIT], OutPoint { txid: split, vout: 4 })?;
    m.deposit(h_resume as u64, 10_000_000);
    let h6 = h_resume + 1;
    assert_state("resume", h6, &m)?;
    assert!(m.b < dowry, "the thaw: drips resume once someone stands again");

    test_log!("[PERMAFROST] drip OK: 1/365 per epoch, e^-1 per year, dowry frozen then thawed");
    Ok(())
}

#[wasm_bindgen_test]
fn permafrost_donate_and_eternity() -> Result<()> {
    clear_test_environment();
    test_log!("\n=== PERMAFROST: tribute via the pot, fixpoint power at giant e ===");

    let mut h: u32 = 892_000;
    // 100 (genesis) + 36.5 (pre-genesis tribute) + 3.65 (live tribute)
    // + 1 (boost) + 10 + 1 (drip triggers) = 152.15 LP.
    let (und_out, nh) = setup_vault(h, 152_150_000_000)?;
    h = nh;
    let anchor = (h - 1) as u64;
    let mut m = Mirror::new(anchor);

    let split = separate(
        h,
        und_out,
        vec![
            ProtostoneEdict { id: rid(&underlying_id()), amount: 100_000_000_000, output: 0 },
            ProtostoneEdict { id: rid(&underlying_id()), amount: 36_500_000_000, output: 1 },
            ProtostoneEdict { id: rid(&underlying_id()), amount: 3_650_000_000, output: 2 },
            ProtostoneEdict { id: rid(&underlying_id()), amount: 10_000_000_000, output: 3 },
            ProtostoneEdict { id: rid(&underlying_id()), amount: 1_000_000_000, output: 4 },
            ProtostoneEdict { id: rid(&underlying_id()), amount: 1_000_000_000, output: 5 },
        ],
        6,
    )?;
    h += 1;

    // ── Boost before genesis: reverts, refunds — no one stands ───
    // The first depositor would swallow it whole; the machine refuses.
    let boost_refund = call_with_input(
        h, vault_id(), vec![OP_DONATE_BOOST],
        OutPoint { txid: split, vout: 5 },
    )?;
    h += 1;
    assert_eq!(
        get_sheet_by_outpoint(&boost_refund)?.get_cached(&rid(&underlying_id())),
        1_000_000_000,
        "a rejected boost must refund in full"
    );
    let (l0, b0_state, s0) = view_state(h)?;
    assert_eq!((l0, b0_state, s0), (0, 0, 0), "a rejected boost must not touch state");

    // ── Donate-pot before genesis: accepted, frozen — a dowry ────
    // No one stands yet, so the release is frozen: the tribute waits
    // whole in the pot for the first generation.
    call_with_input(h, vault_id(), vec![OP_DONATE_POT], OutPoint { txid: split, vout: 1 })?;
    m.donate_pot(h as u64, 36_500_000_000);
    h += 1;
    assert_state("pre-genesis tribute", h, &m)?;
    assert_eq!((m.l, m.b, m.s), (36_500_000_000, 36_500_000_000, 0));

    // ── Genesis ten empty epochs later: the dowry waited whole ───
    let h_gen = (anchor + DRIP_INTERVAL * 10) as u32;
    call_with_input(h_gen, vault_id(), vec![OP_DEPOSIT], OutPoint { txid: split, vout: 0 })?;
    m.deposit(h_gen as u64, 100_000_000_000);
    let h1 = h_gen + 1;
    assert_state("genesis over dowry", h1, &m)?;
    assert_eq!(m.b, 36_500_000_000, "ten empty epochs must not melt the dowry");
    assert_eq!(
        view_u128_args(h1, vec![OP_QUOTE_DEPOSIT, 10_000_000_000])?,
        10_000_000_000,
        "genesis does not capture the dowry: the mint is 1:1, the pot excluded"
    );

    // ── A live-vault donation: rate-neutral, joins the pot ───────
    let (active_before, s_before) = (m.active(), m.s);
    call_with_input(h1, vault_id(), vec![OP_DONATE_POT], OutPoint { txid: split, vout: 2 })?;
    m.donate_pot(h1 as u64, 3_650_000_000);
    let h2 = h1 + 1;
    assert_state("live tribute", h2, &m)?;
    assert_eq!((m.l, m.b, m.s), (140_150_000_000, 40_150_000_000, 100_000_000_000));
    // The tribute lands in the pot, not the rate: nothing to snipe.
    assert_eq!(
        m.active() * s_before,
        active_before * m.s,
        "a pot-donation must not move the rate at the moment of donation"
    );

    // ── The boost: instant, up-only, for those standing now ──────
    // The refunded pre-genesis boost lands successfully on a live vault:
    // L grows, the pot does not, and the rate jumps UP in the same block.
    let (active_before, s_before) = (m.active(), m.s);
    call_with_input(h2, vault_id(), vec![OP_DONATE_BOOST], boost_refund)?;
    m.donate_boost(h2 as u64, 1_000_000_000);
    let h2b = h2 + 1;
    assert_state("boost", h2b, &m)?;
    assert_eq!((m.l, m.b, m.s), (141_150_000_000, 40_150_000_000, 100_000_000_000));
    assert!(
        m.active() * s_before > active_before * m.s,
        "a boost must lift the rate instantly — and can only lift it"
    );
    // Rate is now exactly 1.01: a 10.1 LP deposit mints exactly 10 shares.
    assert_eq!(
        view_u128_args(h2b, vec![OP_QUOTE_DEPOSIT, 10_100_000_000])?,
        10_000_000_000,
        "the boost is in the rate immediately"
    );

    // ── One epoch: the tribute starts dripping — 1/365 released ──
    let h_drip = (anchor + DRIP_INTERVAL * 11) as u32;
    let d_drip = call_with_input(
        h_drip, vault_id(), vec![OP_DEPOSIT],
        OutPoint { txid: split, vout: 3 },
    )?;
    let minted = m.deposit(h_drip as u64, 10_000_000_000);
    let h3 = h_drip + 1;
    assert_state("tribute drips", h3, &m)?;
    assert!(
        m.b == 40_040_000_000 || m.b == 40_040_000_001,
        "one epoch must release pot/365 up to one dust unit: pot = {}",
        m.b
    );
    assert!(minted < 10_000_000_000, "the dripped tribute must lift the rate");
    assert_eq!(get_sheet_by_outpoint(&d_drip)?.get_cached(&rid(&vault_id())), minted);

    // ── Ten quiet years on-chain, then one trigger ───────────────
    // The ceil-discipline drifts UPWARD in B (release floors): the pot
    // decays monotonically toward dust and never underflows, overflows,
    // or resurrects. (The e = 10^6 and u64::MAX extremes are pinned on the
    // power function below — it is the same code on both sides of the
    // mirror; a 144M-block height jump OOMs the test indexer.)
    let h_decade = (anchor + DRIP_INTERVAL * 11 + DRIP_INTERVAL * 3650) as u32;
    call_with_input(
        h_decade, vault_id(), vec![OP_DEPOSIT],
        OutPoint { txid: split, vout: 4 },
    )?;
    m.deposit(h_decade as u64, 1_000_000_000);
    let h4 = h_decade + 1;
    assert_state("ten years", h4, &m)?;
    assert!(
        (1_785_000..=1_800_000).contains(&m.b),
        "ten years must shrink the pot to ~4.478e-5: pot = {}",
        m.b
    );
    assert_eq!(
        view_u128(h4, OP_GET_POT_ANCHOR)?,
        (anchor + DRIP_INTERVAL * 11 + DRIP_INTERVAL * 3650) as u128,
        "the anchor must land exactly on the last consumed epoch boundary"
    );

    // ── The power itself, pinned across the whole range of e ─────
    // Monotone non-increasing in e; ten quiet years leave (364/365)^3650
    // ≈ 4.478e-5 of the pot (slightly under e^-10; the ceil discipline may
    // only keep MORE, never less than the ideal); u64::MAX must not panic
    // and must terminate at the dust unit.
    let b0: u128 = 1_000_000_000_000;
    let samples = [0u64, 1, 10, 365, 3650, 1_000_000, u64::MAX];
    let mut prev = b0;
    for e in samples {
        let cur = pot_after_release(b0, e, YEAR_DAYS);
        assert!(cur <= prev, "pot must be monotone non-increasing in e (e = {})", e);
        prev = cur;
    }
    let ten_years = pot_after_release(b0, 3650, YEAR_DAYS);
    assert!(
        (44_700_000..=44_900_000).contains(&ten_years),
        "ten quiet years must leave ~4.478e-5 of the pot, left {}",
        ten_years
    );
    assert_eq!(
        pot_after_release(b0, 1_000_000, YEAR_DAYS),
        1,
        "a million quiet epochs terminate at one dust unit, not zero"
    );
    assert_eq!(pot_after_release(b0, u64::MAX, YEAR_DAYS), 1, "the power terminates at dust");
    // Degenerate horizons are coherent too: P = 1 dumps the whole pot in
    // one drip; P = 2 halves it (ceil keeps the odd unit).
    assert_eq!(pot_after_release(b0, 1, 1), 0, "P = 1: the first drip empties the pot");
    assert_eq!(pot_after_release(b0, 1, 2), b0 / 2, "P = 2: one drip halves the pot");
    assert_eq!(pot_after_release(1_001, 1, 2), 501, "ceil keeps the odd unit in the pot");

    test_log!("[PERMAFROST] donate+eternity OK: tribute drips like tithes, power safe at any e");
    Ok(())
}

/// Simulate a full Withdraw (release + exit math + storage writes + LP
/// transfer out) at an arbitrary height and return the fuel it burns. The
/// parcel injects the shares as incoming runes; writes land in a discarded
/// overlay, so consecutive measurements see identical state.
fn measure_withdraw_fuel(height: u64, shares: u128) -> Result<u64> {
    let cellpack = Cellpack { target: vault_id(), inputs: vec![OP_WITHDRAW] };
    let parcel = MessageContextParcel {
        atomic: AtomicPointer::default(),
        runes: vec![RuneTransfer { id: rid(&vault_id()), value: shares }],
        transaction: Transaction {
            version: bitcoin::blockdata::transaction::Version::ONE,
            input: vec![],
            output: vec![],
            lock_time: bitcoin::absolute::LockTime::ZERO,
        },
        // The block here is only tx context; the contract reads `height`.
        // The coinbase builder panics on u32::MAX-scale heights, so pin it.
        block: create_block_with_coinbase_tx(height.min(1_000_000) as u32),
        height,
        pointer: 0,
        refund_pointer: 0,
        calldata: cellpack.encipher(),
        sheets: Box::<BalanceSheet<AtomicPointer>>::new(BalanceSheet::default()),
        txindex: 0,
        vout: 0,
        runtime_balances: Box::<BalanceSheet<AtomicPointer>>::new(BalanceSheet::default()),
    };
    let (_resp, gas_used) = simulate_parcel(&parcel, u64::MAX)?;
    Ok(gas_used)
}

#[wasm_bindgen_test]
fn permafrost_release_fuel() -> Result<()> {
    clear_test_environment();
    test_log!("\n=== PERMAFROST: release fuel across quiet-period magnitudes ===");

    // Regtest per-tx minimum fuel — every op must fit even after eternity.
    const FUEL_CAP: u64 = 3_500_000;

    let mut h: u32 = 893_000;
    let (und_out, nh) = setup_vault(h, 136_500_000_000)?;
    h = nh;
    let anchor = (h - 1) as u64;

    let split = separate(
        h,
        und_out,
        vec![
            ProtostoneEdict { id: rid(&underlying_id()), amount: 100_000_000_000, output: 0 },
            ProtostoneEdict { id: rid(&underlying_id()), amount: 36_500_000_000, output: 1 },
        ],
        2,
    )?;
    h += 1;

    // Genesis + fill the pot so the release path has real work to do.
    call_with_input(h, vault_id(), vec![OP_DEPOSIT], OutPoint { txid: split, vout: 0 })?;
    h += 1;
    let d2 = call_with_input(h, vault_id(), vec![OP_DEPOSIT], OutPoint { txid: split, vout: 1 })?;
    h += 1;
    call_with_input(h, vault_id(), vec![OP_WITHDRAW], d2)?;
    h += 1;

    // Fuel of an identical 1e9-share Withdraw after quiet periods of
    // e = 0, 1, 3650 (10 years), 365_000 (1000 years), and the u64 height
    // ceiling (e ≈ 1.28e17, 57 bits — the absolute worst case the type
    // admits; Bitcoin cannot reach it).
    let cases: [(&str, u64); 5] = [
        ("e=0        (same day)", h as u64),
        ("e=1        (next day)", anchor + DRIP_INTERVAL),
        ("e=3650     (10 years)", anchor + DRIP_INTERVAL * 3650),
        ("e=365000 (1000 years)", anchor + DRIP_INTERVAL * 365_000),
        ("e≈1.28e17 (u64::MAX) ", u64::MAX),
    ];
    let mut fuels = [0u64; 5];
    for (i, (label, height)) in cases.iter().enumerate() {
        let fuel = measure_withdraw_fuel(*height, 1_000_000_000)?;
        fuels[i] = fuel;
        test_log!("[PERMAFROST] withdraw fuel {} = {}", label, fuel);
        assert!(
            fuel < FUEL_CAP,
            "{}: fuel {} must fit the {} regtest minimum",
            label, fuel, FUEL_CAP
        );
    }

    // The O(log e) claim, in fuel: the absolute worst case (57 bits of e,
    // ~114 fixpoint multiplications) must cost only marginally more than a
    // same-day exit — well under half the budget in any case.
    let overhead = fuels[4].saturating_sub(fuels[0]);
    test_log!(
        "[PERMAFROST] release overhead at the u64 ceiling: {} fuel ({}% of cap)",
        overhead,
        overhead * 100 / FUEL_CAP
    );
    assert!(
        fuels[4] < FUEL_CAP / 2,
        "even the u64-ceiling release must leave half the budget free: {}",
        fuels[4]
    );

    Ok(())
}
