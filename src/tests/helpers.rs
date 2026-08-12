//! Minimal alkanes test harness for the PERMAFROST vault — the subset of the
//! fire root harness (`src/tests/helpers.rs`) these tests actually use, so
//! the crate is self-contained and can move out of the fire repo wholesale.

pub use alkanes::indexer::index_block;
pub use alkanes::view::simulate_parcel;
pub use alkanes_support::cellpack::Cellpack;
pub use alkanes_support::envelope::RawEnvelope;
pub use alkanes_support::id::AlkaneId;
pub use alkanes_support::response::ExtendedCallResponse;
pub use anyhow::Result;
pub use bitcoin::address::NetworkChecked;
pub use bitcoin::{
    Address, Amount, Block, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
};
pub use metashrew_core::index_pointer::AtomicPointer;
pub use ordinals::Runestone;
pub use protorune::message::MessageContextParcel;
pub use protorune::protostone::Protostones;
pub use protorune::test_helpers::{create_block_with_coinbase_tx, get_address, ADDRESS1};
pub use protorune_support::balance_sheet::BalanceSheet;
pub use protorune_support::network::{set_network, NetworkParams};
pub use protorune_support::protostone::{Protostone, ProtostoneEdict};

// ── Vendored WASMs ───────────────────────────────────────────────

/// The vault under test. Refreshed by `./scripts/build-wasms.sh`.
pub fn get_permafrost_wasm_bytes() -> &'static [u8] {
    include_bytes!("./wasm/permafrost.wasm")
}

/// Mintable test token (fire-master-auth built as a generic token: init
/// opcode 0 mints `units` of itself to the deploy outpoint). Stands in for
/// the underlying token.
pub fn get_mock_token_wasm_bytes() -> &'static [u8] {
    include_bytes!("./wasm/fire_master_auth.wasm")
}

// ── Environment ──────────────────────────────────────────────────

pub fn configure_network() {
    set_network(NetworkParams {
        bech32_prefix: String::from("bcrt"),
        p2pkh_prefix: 0x64,
        p2sh_prefix: 0xc4,
    });
}

pub fn clear_test_environment() {
    metashrew_core::clear();
    configure_network();
    for height in 0..3 {
        let block = create_block_with_coinbase_tx(height);
        index_block(&block, height).expect("Failed to index empty block");
    }
}

// ── Views ────────────────────────────────────────────────────────

pub fn simulate_cellpack(height: u64, cellpack: Cellpack) -> Result<(ExtendedCallResponse, u64)> {
    let parcel = MessageContextParcel {
        atomic: AtomicPointer::default(),
        runes: vec![],
        transaction: Transaction {
            version: bitcoin::blockdata::transaction::Version::ONE,
            input: vec![],
            output: vec![],
            lock_time: bitcoin::absolute::LockTime::ZERO,
        },
        block: create_block_with_coinbase_tx(height as u32),
        height,
        pointer: 0,
        refund_pointer: 0,
        calldata: cellpack.encipher(),
        sheets: Box::<BalanceSheet<AtomicPointer>>::new(BalanceSheet::default()),
        txindex: 0,
        vout: 0,
        runtime_balances: Box::<BalanceSheet<AtomicPointer>>::new(BalanceSheet::default()),
    };
    simulate_parcel(&parcel, u64::MAX)
}

pub fn get_sheet_by_outpoint(
    outpoint: &OutPoint,
) -> Result<BalanceSheet<metashrew_core::index_pointer::IndexPointer>> {
    use alkanes::message::AlkaneMessageContext;
    use metashrew_support::index_pointer::KeyValuePointer;
    use metashrew_support::utils::consensus_encode;
    use protorune::balance_sheet::load_sheet;
    use protorune::message::MessageContext;
    use protorune::tables::RuneTable;

    let ptr = RuneTable::for_protocol(AlkaneMessageContext::protocol_tag())
        .OUTPOINT_TO_RUNES
        .select(&consensus_encode(outpoint)?);
    Ok(load_sheet(&ptr))
}

// ── Transaction builders ─────────────────────────────────────────

pub fn create_multiple_cellpack_with_witness(
    witness: Witness,
    cellpacks: Vec<Cellpack>,
    etch: bool,
) -> Transaction {
    let txin = TxIn {
        previous_output: OutPoint::null(),
        script_sig: ScriptBuf::new(),
        sequence: Sequence::MAX,
        witness,
    };
    create_multiple_cellpack_with_witness_and_txins_edicts(cellpacks, vec![txin], etch, vec![])
}

pub fn create_multiple_cellpack_with_witness_and_in(
    witness: Witness,
    cellpacks: Vec<Cellpack>,
    previous_output: OutPoint,
    etch: bool,
) -> Transaction {
    let txin = TxIn {
        previous_output,
        script_sig: ScriptBuf::new(),
        sequence: Sequence::MAX,
        witness,
    };
    create_multiple_cellpack_with_witness_and_txins_edicts(cellpacks, vec![txin], etch, vec![])
}

pub fn create_multiple_cellpack_with_witness_and_txins_edicts(
    cellpacks: Vec<Cellpack>,
    txins: Vec<TxIn>,
    _etch: bool,
    edicts: Vec<ProtostoneEdict>,
) -> Transaction {
    let protostones: Vec<Protostone> = cellpacks
        .into_iter()
        .map(|cellpack| Protostone {
            message: cellpack.encipher(),
            pointer: Some(0),
            refund: Some(0),
            edicts: edicts.clone(),
            from: None,
            burn: None,
            protocol_tag: 1,
        })
        .collect();

    let runestone: ScriptBuf = (Runestone {
        etching: None,
        pointer: Some(0),
        edicts: Vec::new(),
        mint: None,
        protocol: protostones.encipher().ok(),
    })
    .encipher();

    let address: Address<NetworkChecked> = get_address(&ADDRESS1().as_str());
    let op_return = TxOut {
        value: Amount::from_sat(0),
        script_pubkey: runestone,
    };
    let recipient_output = TxOut {
        value: Amount::from_sat(100_000_000),
        script_pubkey: address.script_pubkey(),
    };

    let inputs = if txins.is_empty() {
        vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }]
    } else {
        txins
    };

    Transaction {
        version: bitcoin::blockdata::transaction::Version::ONE,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: inputs,
        output: vec![recipient_output, op_return],
    }
}

/// Deploy binaries with paired cellpacks in one block (each tx chains the
/// previous tx's vout 0).
pub fn init_with_multiple_cellpacks_with_tx(
    binaries: Vec<Vec<u8>>,
    cellpacks: Vec<Cellpack>,
) -> Block {
    let block_height = 880_000;
    let mut test_block = create_block_with_coinbase_tx(block_height);
    let mut previous_out: Option<OutPoint> = None;

    let mut txs = binaries
        .into_iter()
        .zip(cellpacks.into_iter())
        .map(|(binary, cellpack)| {
            let witness = if binary.is_empty() {
                Witness::new()
            } else {
                RawEnvelope::from(binary).to_witness(true)
            };

            let tx = if let Some(previous_output) = previous_out {
                create_multiple_cellpack_with_witness_and_in(
                    witness,
                    vec![cellpack],
                    previous_output,
                    false,
                )
            } else {
                create_multiple_cellpack_with_witness(witness, vec![cellpack], false)
            };
            previous_out = Some(OutPoint {
                txid: tx.compute_txid(),
                vout: 0,
            });
            tx
        })
        .collect::<Vec<Transaction>>();

    test_block.txdata.append(&mut txs);
    test_block
}

/// Deploy the mock token at `4:slot` and mint `units` to the returned
/// outpoint.
pub fn deploy_mock_token(height: u32, slot: u128, units: u128) -> Result<OutPoint> {
    let mut block = create_block_with_coinbase_tx(height);
    let witness = RawEnvelope::from(get_mock_token_wasm_bytes().to_vec()).to_witness(true);
    let cellpack = Cellpack {
        target: AlkaneId { block: 3, tx: slot },
        inputs: vec![0, units],
    };
    let tx = create_multiple_cellpack_with_witness(witness, vec![cellpack], false);
    let txid = tx.compute_txid();
    block.txdata.push(tx);
    index_block(&block, height)?;
    Ok(OutPoint { txid, vout: 0 })
}
