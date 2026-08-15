//! Mainnet deploy rehearsal — drives the EXACT production cellpack from
//! deploy/deploy-permafrost.sh through the in-process metashrew VM.
//!
//! Every number below is a literal copied from the deploy script (not
//! derived via pack_str), so this test verifies the broadcast values
//! themselves: slot 4:365, underlying 4:1778, 10% tithe, 144-block drip,
//! 365-drip horizon, name "PERMAFROST", symbol "FROST" as packed u128s.
//! If the script and this test ever disagree, one of them is wrong.

use crate::tests::helpers::*;
use crate::tests::test_log;

// ── Literals from deploy/deploy-permafrost.sh ──────────────────────────
const SLOT: u128 = 365;
const UNDERLYING_BLOCK: u128 = 4;
const UNDERLYING_TX: u128 = 1778;
const PENALTY_BPS: u128 = 1000;
const NAME_U128: u128 = 398215580027291461371216; // "PERMAFROST"
const SYMBOL_U128: u128 = 362174960198; // "FROST"
const INTERVAL: u128 = 144;
const PERIODS: u128 = 365;

fn vault() -> AlkaneId {
    AlkaneId { block: 4, tx: SLOT }
}

fn view(height: u32, inputs: Vec<u128>) -> Result<Vec<u8>> {
    let (resp, _) = simulate_cellpack(height as u64, Cellpack { target: vault(), inputs })?;
    Ok(resp.data)
}

fn view_u128(height: u32, opcode: u128) -> Result<u128> {
    let data = view(height, vec![opcode])?;
    anyhow::ensure!(data.len() >= 16, "view returned {} bytes", data.len());
    let mut b = [0u8; 16];
    b.copy_from_slice(&data[0..16]);
    Ok(u128::from_le_bytes(b))
}

#[wasm_bindgen_test::wasm_bindgen_test]
fn mainnet_deploy_rehearsal_4_365() -> Result<()> {
    clear_test_environment();
    test_log!("\n=== DEPLOY REHEARSAL: production cellpack at 4:365 ===");

    let mut h: u32 = 909_000;

    // Stand in for the BTCUSD pool's LP token at the real id 4:1778.
    let und_out = deploy_mock_token(h, UNDERLYING_TX, 100_000_000)?;
    h += 1;

    // The production protostone: CREATERESERVED [3,365] + Initialize, verbatim.
    let block = init_with_multiple_cellpacks_with_tx(
        vec![get_permafrost_wasm_bytes().to_vec()],
        vec![Cellpack {
            target: AlkaneId { block: 3, tx: SLOT },
            inputs: vec![
                0, // Initialize
                UNDERLYING_BLOCK,
                UNDERLYING_TX,
                PENALTY_BPS,
                NAME_U128,
                SYMBOL_U128,
                INTERVAL,
                PERIODS,
            ],
        }],
    );
    index_block(&block, h)?;
    h += 1;

    // Post-deploy checks — the same list the deploy script prints.
    let name = String::from_utf8_lossy(&view(h, vec![99])?).to_string();
    assert_eq!(name, "PERMAFROST", "opcode 99: packed name must decode");
    let symbol = String::from_utf8_lossy(&view(h, vec![100])?).to_string();
    assert_eq!(symbol, "FROST", "opcode 100: packed symbol must decode");
    assert_eq!(view_u128(h, 103)?, PENALTY_BPS, "opcode 103: tithe bps");
    assert_eq!(view_u128(h, 110)?, INTERVAL, "opcode 110: drip interval");
    assert_eq!(view_u128(h, 111)?, PERIODS, "opcode 111: release periods");
    let und = view(h, vec![104])?;
    anyhow::ensure!(und.len() >= 32, "opcode 104 returned {} bytes", und.len());
    let mut ub = [0u8; 16];
    let mut ut = [0u8; 16];
    ub.copy_from_slice(&und[0..16]);
    ut.copy_from_slice(&und[16..32]);
    assert_eq!(
        (u128::from_le_bytes(ub), u128::from_le_bytes(ut)),
        (UNDERLYING_BLOCK, UNDERLYING_TX),
        "opcode 104: underlying id"
    );

    // Genesis smoke: first deposit (spending the minted underlying outpoint)
    // burns the 1000 dead shares and credits L in full.
    let deposit_amount = 100_000_000u128; // the full minted outpoint
    let mut block = create_block_with_coinbase_tx(h);
    let tx = create_multiple_cellpack_with_witness_and_in(
        Witness::new(),
        vec![Cellpack { target: vault(), inputs: vec![1] }],
        und_out,
        false,
    );
    block.txdata.push(tx);
    index_block(&block, h)?;
    h += 1;
    assert_eq!(
        view_u128(h, 101)?,
        deposit_amount,
        "genesis: S = full deposit (dead shares included)"
    );
    assert_eq!(view_u128(h, 102)?, deposit_amount, "genesis: L = deposit");

    test_log!(
        "rehearsal OK: 4:365 initialized as PERMAFROST/FROST wrapping 4:1778, genesis deposit accepted"
    );
    Ok(())
}
