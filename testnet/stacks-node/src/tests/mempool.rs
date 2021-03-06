
use stacks::vm::{Value, types::PrincipalData};
use stacks::chainstate::burn::{BlockHeaderHash};
use stacks::address::AddressHashMode;
use stacks::net::{Error as NetError, StacksMessageCodec};
use stacks::util::{secp256k1::*, hash::*};

use stacks::chainstate::stacks::{
    StacksBlockHeader,
    Error as ChainstateError,
    db::blocks::MemPoolRejection,
    C32_ADDRESS_VERSION_MAINNET_SINGLESIG,
    StacksMicroblockHeader, StacksPrivateKey, TransactionSpendingCondition, TransactionAuth, TransactionVersion,
    StacksPublicKey, TransactionPayload, StacksTransactionSigner,
    TokenTransferMemo,
    StacksTransaction, StacksAddress };


use crate::Keychain;
use crate::helium::RunLoop;

use crate::node::TESTNET_CHAIN_ID;

use super::{SK_1, SK_2, make_contract_publish, to_addr, make_contract_call, make_stacks_transfer, make_poison, make_coinbase};

const FOO_CONTRACT: &'static str = "(define-public (foo) (ok 1))
                                    (define-public (bar (x uint)) (ok x))";

pub fn make_bad_stacks_transfer(sender: &StacksPrivateKey, nonce: u64, fee_rate: u64,
                                recipient: &PrincipalData, amount: u64) -> Vec<u8> {
    let payload = TransactionPayload::TokenTransfer(recipient.clone(), amount, TokenTransferMemo([0; 34]));

    let mut spending_condition = TransactionSpendingCondition::new_singlesig_p2pkh(StacksPublicKey::from_private(sender))
        .expect("Failed to create p2pkh spending condition from public key.");
    spending_condition.set_nonce(nonce);
    spending_condition.set_fee_rate(fee_rate);
    let auth = TransactionAuth::Standard(spending_condition);
    
    let mut unsigned_tx = StacksTransaction::new(TransactionVersion::Testnet, auth, payload);
    unsigned_tx.chain_id = TESTNET_CHAIN_ID;

    let mut tx_signer = StacksTransactionSigner::new(&unsigned_tx);

    tx_signer.sign_origin(&StacksPrivateKey::new()).unwrap();

    let mut buf = vec![];
    tx_signer.get_tx().unwrap().consensus_serialize(&mut buf).unwrap();
    buf
}

#[test]
fn mempool_setup_chainstate() {
    let mut conf = super::new_test_conf();
    
    // force seeds to be the same
    conf.node.seed = vec![0x00];

    conf.burnchain.commit_anchor_block_within = 1500;
    
    let contract_sk = StacksPrivateKey::from_hex(SK_1).unwrap();
    let contract_addr = to_addr(&contract_sk);
    conf.add_initial_balance(contract_addr.to_string(), 100000);

    let num_rounds = 4;

    let mut run_loop = RunLoop::new(conf.clone());

    run_loop.callbacks.on_new_tenure(|round, _burnchain_tip, chain_tip, tenure| {
        let contract_sk = StacksPrivateKey::from_hex(SK_1).unwrap();
        let header_hash = chain_tip.block.block_hash();
        let burn_header_hash = chain_tip.metadata.burn_header_hash;

        if round == 1 {
            let publish_tx = make_contract_publish(&contract_sk, 0, 100, "foo_contract", FOO_CONTRACT);
            eprintln!("Tenure in 1 started!");
        
            tenure.mem_pool.submit_raw(&burn_header_hash, &header_hash, publish_tx).unwrap();
        }
    });

    run_loop.callbacks.on_new_stacks_chain_state(|round, _burnchain_tip, chain_tip, chain_state| {
        let contract_sk = StacksPrivateKey::from_hex(SK_1).unwrap();
        let contract_addr = to_addr(&contract_sk);

        let other_sk =  StacksPrivateKey::from_hex(SK_2).unwrap();
        let other_addr = to_addr(&other_sk).into();

        if round == 3 {
            let block_header = chain_tip.metadata.clone();
            let burn_hash = &block_header.burn_header_hash;
            let block_hash = &block_header.anchored_header.block_hash();

            let micro_pubkh = &block_header.anchored_header.microblock_pubkey_hash;

            // let's throw some transactions at it.
            // first a couple valid ones:
            let tx_bytes = make_contract_publish(&contract_sk, 1, 1000, "bar_contract", FOO_CONTRACT);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap();

            let tx_bytes = make_contract_call(&contract_sk, 1, 200, &contract_addr, "foo_contract", "bar", &[Value::UInt(1)]);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap();

            let tx_bytes = make_stacks_transfer(&contract_sk, 1, 200, &other_addr, 1000);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap();

            // bad signature
            let tx_bytes = make_bad_stacks_transfer(&contract_sk, 1, 200, &other_addr, 1000);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            let e = chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap_err();
            eprintln!("Err: {:?}", e);
            assert!(if let
                    MemPoolRejection::FailedToValidate(
                        ChainstateError::NetError(NetError::VerifyingError(_))) = e { true } else { false });

            // mismatched network on contract-call!
            let bad_addr = 
                StacksAddress::from_public_keys(
                    88, &AddressHashMode::SerializeP2PKH, 1, &vec![StacksPublicKey::from_private(&other_sk)])
                .unwrap()
                .into();

            let tx_bytes = make_contract_call(&contract_sk, 1, 200, &bad_addr, "foo_contract", "bar", &[Value::UInt(1), Value::Int(2)]);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            let e = chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap_err();

            assert!(if let MemPoolRejection::BadAddressVersionByte = e { true } else { false });

            // mismatched network on transfer!
            let bad_addr = 
                StacksAddress::from_public_keys(
                    C32_ADDRESS_VERSION_MAINNET_SINGLESIG, &AddressHashMode::SerializeP2PKH, 1, &vec![StacksPublicKey::from_private(&other_sk)])
                .unwrap()
                .into();

            let tx_bytes = make_stacks_transfer(&contract_sk, 1, 200, &bad_addr, 1000);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            let e = chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap_err();
            assert!(if let MemPoolRejection::BadAddressVersionByte = e { true } else { false });

            // bad fees
            let tx_bytes = make_stacks_transfer(&contract_sk, 1, 0, &other_addr, 1000);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            let e = chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap_err(); 
            eprintln!("Err: {:?}", e);
            assert!(if let MemPoolRejection::FeeTooLow(0, _) = e { true } else { false });

            // bad nonce
            let tx_bytes = make_stacks_transfer(&contract_sk, 0, 200, &other_addr, 1000);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            let e = chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap_err(); 
            eprintln!("Err: {:?}", e);
            assert!(if let MemPoolRejection::BadNonces(_) = e { true } else { false });

            // not enough funds
            let tx_bytes = make_stacks_transfer(&contract_sk, 1, 110000, &other_addr, 1000);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            let e = chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap_err(); 
            eprintln!("Err: {:?}", e);
            assert!(if let MemPoolRejection::NotEnoughFunds(111000, 99900) = e { true } else { false });

            let tx_bytes = make_stacks_transfer(&contract_sk, 1, 99900, &other_addr, 1000);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            let e = chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap_err(); 
            eprintln!("Err: {:?}", e);
            assert!(if let MemPoolRejection::NotEnoughFunds(100900, 99900) = e { true } else { false });

            let tx_bytes = make_contract_call(&contract_sk, 1, 200, &contract_addr, "bar_contract", "bar", &[Value::UInt(1)]);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            let e = chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap_err(); 
            eprintln!("Err: {:?}", e);
            assert!(if let MemPoolRejection::NoSuchContract = e { true } else { false });

            let tx_bytes = make_contract_call(&contract_sk, 1, 200, &contract_addr, "foo_contract", "foobar", &[Value::UInt(1)]);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            let e = chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap_err(); 
            eprintln!("Err: {:?}", e);
            assert!(if let MemPoolRejection::NoSuchPublicFunction = e { true } else { false });

            let tx_bytes = make_contract_call(&contract_sk, 1, 200, &contract_addr, "foo_contract", "bar", &[Value::UInt(1), Value::Int(2)]);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            let e = chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap_err(); 
            eprintln!("Err: {:?}", e);
            assert!(if let MemPoolRejection::BadFunctionArgument(_) = e { true } else { false });

            let tx_bytes = make_contract_publish(&contract_sk, 1, 1000, "foo_contract", FOO_CONTRACT);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            let e = chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap_err(); 
            eprintln!("Err: {:?}", e);
            assert!(if let MemPoolRejection::ContractAlreadyExists(_) = e { true } else { false });

            let microblock_1 = StacksMicroblockHeader {
                version: 0,
                sequence: 0,
                prev_block: BlockHeaderHash([0; 32]),
                tx_merkle_root: Sha512Trunc256Sum::from_data(&[]),
                signature: MessageSignature([1; 65])
            };

            let microblock_2 = StacksMicroblockHeader {
                version: 0,
                sequence: 1,
                prev_block: BlockHeaderHash([0; 32]),
                tx_merkle_root: Sha512Trunc256Sum::from_data(&[]),
                signature: MessageSignature([1; 65])
            };

            let tx_bytes = make_poison(&contract_sk, 1, 1000, microblock_1, microblock_2);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            let e = chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap_err(); 
            eprintln!("Err: {:?}", e);
            assert!(if let MemPoolRejection::PoisonMicroblocksDoNotConflict = e { true } else { false });

            let microblock_1 = StacksMicroblockHeader {
                version: 0,
                sequence: 0,
                prev_block: block_hash.clone(),
                tx_merkle_root: Sha512Trunc256Sum::from_data(&[]),
                signature: MessageSignature([0; 65])
            };

            let microblock_2 = StacksMicroblockHeader {
                version: 0,
                sequence: 0,
                prev_block: block_hash.clone(),
                tx_merkle_root: Sha512Trunc256Sum::from_data(&[1,2,3]),
                signature: MessageSignature([0; 65])
            };

            let tx_bytes = make_poison(&contract_sk, 1, 1000, microblock_1, microblock_2);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            let e = chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap_err(); 
            eprintln!("Err: {:?}", e);
            assert!(if let MemPoolRejection::InvalidMicroblocks = e { true } else { false });


            let mut microblock_1 = StacksMicroblockHeader {
                version: 0,
                sequence: 0,
                prev_block: BlockHeaderHash([0; 32]),
                tx_merkle_root: Sha512Trunc256Sum::from_data(&[]),
                signature: MessageSignature([0; 65])
            };

            let mut microblock_2 = StacksMicroblockHeader {
                version: 0,
                sequence: 0,
                prev_block: BlockHeaderHash([0; 32]),
                tx_merkle_root: Sha512Trunc256Sum::from_data(&[1,2,3]),
                signature: MessageSignature([0; 65])
            };

            microblock_1.sign(&other_sk).unwrap();
            microblock_2.sign(&other_sk).unwrap();

            let tx_bytes = make_poison(&contract_sk, 1, 1000, microblock_1, microblock_2);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            let e = chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap_err(); 
            eprintln!("Err: {:?}", e);
            assert!(if let MemPoolRejection::NoAnchorBlockWithPubkeyHash(_) = e { true } else { false });

            let tx_bytes = make_coinbase(&contract_sk, 1, 1000);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            let e = chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap_err(); 
            eprintln!("Err: {:?}", e);
            assert!(if let MemPoolRejection::NoCoinbaseViaMempool = e { true } else { false });

            // find the correct priv-key
            let mut secret_key = None;
            let mut conf = super::new_test_conf();
            conf.node.seed = vec![0x00];

            let mut keychain = Keychain::default(conf.node.seed.clone());
            for _i in 0..4 {
                let microblock_secret_key = keychain.rotate_microblock_keypair();
                let mut microblock_pubkey = Secp256k1PublicKey::from_private(&microblock_secret_key);
                microblock_pubkey.set_compressed(true);
                let pubkey_hash = StacksBlockHeader::pubkey_hash(&microblock_pubkey);
                if pubkey_hash == *micro_pubkh {
                    secret_key = Some(microblock_secret_key);
                    break;
                }
            }

            let secret_key = secret_key.expect("Failed to find the microblock secret key");

            let mut microblock_1 = StacksMicroblockHeader {
                version: 0,
                sequence: 0,
                prev_block: BlockHeaderHash([0; 32]),
                tx_merkle_root: Sha512Trunc256Sum::from_data(&[]),
                signature: MessageSignature([0; 65])
            };

            let mut microblock_2 = StacksMicroblockHeader {
                version: 0,
                sequence: 0,
                prev_block: BlockHeaderHash([0; 32]),
                tx_merkle_root: Sha512Trunc256Sum::from_data(&[1,2,3]),
                signature: MessageSignature([0; 65])
            };

            microblock_1.sign(&secret_key).unwrap();
            microblock_2.sign(&secret_key).unwrap();

            let tx_bytes = make_poison(&contract_sk, 1, 1000, microblock_1, microblock_2);
            let tx = StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
            chain_state.will_admit_mempool_tx(burn_hash, block_hash, &tx, tx_bytes.len() as u64).unwrap(); 
        }
    });

    run_loop.start(num_rounds);
}
