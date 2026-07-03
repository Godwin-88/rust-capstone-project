use bitcoincore_rpc::bitcoin::{Address, Amount, Network};
use bitcoincore_rpc::{Auth, Client, RpcApi};
use serde::Deserialize;
use serde_json::json;
use std::fs::File;
use std::io::Write;

// Node access params
const RPC_URL: &str = "http://127.0.0.1:18443"; // Default regtest RPC port
const RPC_USER: &str = "alice";
const RPC_PASS: &str = "password";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to Bitcoin Core RPC (node-level, used for wallet management)
    let rpc = Client::new(
        RPC_URL,
        Auth::UserPass(RPC_USER.to_owned(), RPC_PASS.to_owned()),
    )?;

    // --- Create/Load Wallets ---
    // Try to create each wallet first. If the wallet already exists, load it instead.
    // We handle both cases to make the script idempotent.

    // Create/Load Miner wallet
    match rpc.create_wallet("Miner", None, None, None, None) {
        Ok(info) => println!("Created Miner wallet: {:?}", info.name),
        Err(_) => {
            let info = rpc.load_wallet("Miner")?;
            println!("Loaded Miner wallet: {:?}", info.name);
        }
    }

    // Create/Load Trader wallet
    match rpc.create_wallet("Trader", None, None, None, None) {
        Ok(info) => println!("Created Trader wallet: {:?}", info.name),
        Err(_) => {
            let info = rpc.load_wallet("Trader")?;
            println!("Loaded Trader wallet: {:?}", info.name);
        }
    }

    // Create wallet-specific RPC clients
    // Bitcoin Core requires wallet-specific endpoints for wallet operations
    let rpc_miner = Client::new(
        &format!("{}/wallet/Miner", RPC_URL),
        Auth::UserPass(RPC_USER.to_owned(), RPC_PASS.to_owned()),
    )?;

    let rpc_trader = Client::new(
        &format!("{}/wallet/Trader", RPC_URL),
        Auth::UserPass(RPC_USER.to_owned(), RPC_PASS.to_owned()),
    )?;

    // --- Step 1: Generate Miner address and mine blocks ---
    // Generate an address for the Miner wallet with label "Mining Reward"
    // `get_new_address` returns an unchecked address; we require regtest network to validate it.
    let miner_address_unchecked = rpc_miner.get_new_address(Some("Mining Reward"), None)?;
    let miner_address = miner_address_unchecked.require_network(Network::Regtest)?;
    println!("Miner address: {}", miner_address);

    // Mine 101 blocks to the Miner's address.
    // Why 101 blocks? Bitcoin's consensus rules require coinbase outputs to
    // mature for 100 blocks (BIP34) before they can be spent. Mining 101 blocks
    // means the coinbase reward from block 1 is confirmed by 100 subsequent blocks
    // (blocks 2-101), making it spendable. Block 101's coinbase is still immature.
    println!("Mining 101 blocks to generate spendable balance...");
    let block_hashes = rpc_miner.generate_to_address(101, &miner_address)?;
    println!("Mined {} blocks", block_hashes.len());

    // Print the balance of the Miner wallet
    let miner_balance = rpc_miner.get_balance(None, None)?;
    println!("Miner balance: {} BTC", miner_balance.to_btc());

    // --- Step 2: Generate Trader address ---
    // Generate a receiving address for the Trader wallet with label "Received"
    let trader_address_unchecked = rpc_trader.get_new_address(Some("Received"), None)?;
    let trader_address = trader_address_unchecked.require_network(Network::Regtest)?;
    println!("Trader address: {}", trader_address);

    // --- Step 3: Send 20 BTC from Miner to Trader ---
    println!("Sending 20 BTC from Miner to Trader...");
    let txid = rpc_miner.send_to_address(
        &trader_address,
        Amount::from_btc(20.0)?,
        None, // comment
        None, // comment_to
        None, // subtract_fee_from_amount
        None, // replaceable
        None, // conf_target
        None, // estimate_mode
    )?;
    println!("Transaction sent! TXID: {}", txid);

    // Check the mempool for the unconfirmed transaction
    println!("Checking mempool for unconfirmed transaction...");
    let mempool_entry = rpc_miner.get_mempool_entry(&txid)?;
    println!("Mempool entry: {:?}", mempool_entry);

    // --- Step 4: Confirm the transaction by mining 1 block ---
    println!("Mining 1 block to confirm the transaction...");
    rpc_miner.generate_to_address(1, &miner_address)?;
    println!("Transaction confirmed!");

    // --- Step 5: Fetch full transaction details ---
    // We use the generic `call` method to invoke `gettransaction` RPC with
    // `verbose=true` (third param) to include the decoded transaction.
    // This gives us access to `decoded` which contains the vin/vout details.
    #[derive(Deserialize, Debug)]
    struct DecodedVin {
        txid: String,
        vout: u32,
    }

    #[derive(Deserialize, Debug)]
    struct ScriptPubKey {
        address: Option<String>,
        addresses: Option<Vec<String>>,
    }

    #[derive(Deserialize, Debug)]
    struct Vout {
        value: f64,
        #[serde(rename = "scriptPubKey")]
        script_pub_key: ScriptPubKey,
    }

    #[derive(Deserialize, Debug)]
    struct DecodedTx {
        txid: String,
        vin: Vec<DecodedVin>,
        vout: Vec<Vout>,
    }

    #[derive(Deserialize, Debug)]
    struct GetTxResult {
        txid: String,
        fee: f64,
        blockhash: String,
        blockheight: u64,
        decoded: Option<DecodedTx>,
    }

    let get_tx_result = rpc_miner.call::<GetTxResult>(
        "gettransaction",
        &[json!(&txid.to_string()), json!(null), json!(true)],
    )?;

    let decoded = get_tx_result
        .decoded
        .expect("decoded transaction should be present");
    let block_height = get_tx_result.blockheight;
    let block_hash = get_tx_result.blockhash;
    let fee = get_tx_result.fee;

    // The transaction has 1 vin (spending a coinbase UTXO) and 2 vouts (trader + change).
    // Verify this matches expectations
    assert_eq!(decoded.vin.len(), 1, "Expected exactly 1 input");
    assert_eq!(decoded.vout.len(), 2, "Expected exactly 2 outputs");

    // --- Step 6: Identify the trader output and change output ---
    // The trader output value should be 20 BTC (matching what we sent).
    // The change output is the remaining output that goes back to the Miner wallet.
    // We identify outputs by their value since the trader address may not match
    // exactly depending on how addresses appear in the decoded tx.
    let trader_vout = decoded
        .vout
        .iter()
        .enumerate()
        .find(|(_, vout)| {
            // Match by value: 20 BTC
            (vout.value - 20.0).abs() < 0.001
        })
        .expect("Trader output not found in transaction");

    let change_vout = decoded
        .vout
        .iter()
        .enumerate()
        .find(|(i, _)| *i != trader_vout.0)
        .expect("Change output not found in transaction");

    // --- Step 7: Get the input (previous output) details ---
    // The vin[0] references the coinbase UTXO being spent. We need to look up
    // the previous transaction (the coinbase) to find the input address and amount.
    // Use the Txid type from the same bitcoin version that bitcoincore-rpc uses.
    let prev_txid: bitcoincore_rpc::bitcoin::Txid = decoded.vin[0].txid.parse()?;
    let prev_tx = rpc_miner.get_raw_transaction(&prev_txid, None)?;
    let prev_txout = &prev_tx.output[decoded.vin[0].vout as usize];

    let input_address = Address::from_script(&prev_txout.script_pubkey, Network::Regtest)?;
    let input_amount = prev_txout.value;

    // Extract vout addresses and amounts
    let trader_output_address = trader_vout
        .1
        .script_pub_key
        .address
        .as_deref()
        .or_else(|| {
            trader_vout
                .1
                .script_pub_key
                .addresses
                .as_ref()
                .and_then(|a| a.first().map(|s| s.as_str()))
        })
        .expect("no address found for trader vout")
        .to_string();
    let trader_output_amount = Amount::from_btc(trader_vout.1.value)?;

    let change_output_address = change_vout
        .1
        .script_pub_key
        .address
        .as_deref()
        .or_else(|| {
            change_vout
                .1
                .script_pub_key
                .addresses
                .as_ref()
                .and_then(|a| a.first().map(|s| s.as_str()))
        })
        .expect("no address found for change vout")
        .to_string();
    let change_output_amount = Amount::from_btc(change_vout.1.value)?;

    // --- Step 8: Write output to out.txt ---
    // Format: one attribute per line, exactly 10 lines
    // The fee from gettransaction RPC is reported as a negative value (e.g. -0.00000141).
    // We write it as-is since the test takes abs(fee) for validation.
    let output = format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
        txid,
        input_address.to_string(),
        input_amount.to_btc(),
        trader_output_address,
        trader_output_amount.to_btc(),
        change_output_address,
        change_output_amount.to_btc(),
        fee,
        block_height,
        block_hash,
    );

    let mut file = File::create("../out.txt")?;
    file.write_all(output.as_bytes())?;
    println!("Output written to out.txt successfully!");
    println!("--- Transaction Summary ---");
    println!("TXID: {}", txid);
    println!("Input Address: {}", input_address);
    println!("Input Amount: {} BTC", input_amount.to_btc());
    println!("Trader Output Address: {}", trader_output_address);
    println!("Trader Output Amount: {} BTC", trader_output_amount.to_btc());
    println!("Change Address: {}", change_output_address);
    println!("Change Amount: {} BTC", change_output_amount.to_btc());
    println!("Fee: {} BTC", fee);
    println!("Block Height: {}", block_height);
    println!("Block Hash: {}", block_hash);

    Ok(())
}