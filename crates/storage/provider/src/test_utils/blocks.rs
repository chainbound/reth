//! Dummy blocks and data for tests

use crate::{post_state::PostState, DatabaseProviderRW};
use reth_db::{database::Database, models::StoredBlockBodyIndices, tables};
use reth_primitives::{
    hex_literal::hex, Account, BlockNumber, Bytes, Header, Log, Receipt, SealedBlock,
    SealedBlockWithSenders, TxType, Withdrawal, H160, H256, U256,
};
use reth_rlp::Decodable;
use std::collections::BTreeMap;

/// Assert genesis block
pub fn assert_genesis_block<DB: Database>(provider: &DatabaseProviderRW<'_, DB>, g: SealedBlock) {
    let n = g.number;
    let h = H256::zero();
    let tx = provider;

    // check if all tables are empty
    assert_eq!(tx.table::<tables::Headers>().unwrap(), vec![(g.number, g.header.clone().unseal())]);

    assert_eq!(tx.table::<tables::HeaderNumbers>().unwrap(), vec![(h, n)]);
    assert_eq!(tx.table::<tables::CanonicalHeaders>().unwrap(), vec![(n, h)]);
    assert_eq!(tx.table::<tables::HeaderTD>().unwrap(), vec![(n, g.difficulty.into())]);
    assert_eq!(
        tx.table::<tables::BlockBodyIndices>().unwrap(),
        vec![(0, StoredBlockBodyIndices::default())]
    );
    assert_eq!(tx.table::<tables::BlockOmmers>().unwrap(), vec![]);
    assert_eq!(tx.table::<tables::BlockWithdrawals>().unwrap(), vec![]);
    assert_eq!(tx.table::<tables::Transactions>().unwrap(), vec![]);
    assert_eq!(tx.table::<tables::TransactionBlock>().unwrap(), vec![]);
    assert_eq!(tx.table::<tables::TxHashNumber>().unwrap(), vec![]);
    assert_eq!(tx.table::<tables::Receipts>().unwrap(), vec![]);
    assert_eq!(tx.table::<tables::PlainAccountState>().unwrap(), vec![]);
    assert_eq!(tx.table::<tables::PlainStorageState>().unwrap(), vec![]);
    assert_eq!(tx.table::<tables::AccountHistory>().unwrap(), vec![]);
    assert_eq!(tx.table::<tables::StorageHistory>().unwrap(), vec![]);
    // TODO check after this gets done: https://github.com/paradigmxyz/reth/issues/1588
    // Bytecodes are not reverted assert_eq!(tx.table::<tables::Bytecodes>().unwrap(), vec![]);
    assert_eq!(tx.table::<tables::AccountChangeSet>().unwrap(), vec![]);
    assert_eq!(tx.table::<tables::StorageChangeSet>().unwrap(), vec![]);
    assert_eq!(tx.table::<tables::HashedAccount>().unwrap(), vec![]);
    assert_eq!(tx.table::<tables::HashedStorage>().unwrap(), vec![]);
    assert_eq!(tx.table::<tables::AccountsTrie>().unwrap(), vec![]);
    assert_eq!(tx.table::<tables::StoragesTrie>().unwrap(), vec![]);
    assert_eq!(tx.table::<tables::TxSenders>().unwrap(), vec![]);
    // SyncStage is not updated in tests
}

/// Test chain with genesis, blocks, execution results
/// that have valid changesets.
pub struct BlockChainTestData {
    /// Genesis
    pub genesis: SealedBlock,
    /// Blocks with its execution result
    pub blocks: Vec<(SealedBlockWithSenders, PostState)>,
}

impl BlockChainTestData {
    /// Create test data with two blocks that are connected, specifying their block numbers.
    pub fn default_with_numbers(one: BlockNumber, two: BlockNumber) -> Self {
        let one = block1(one);
        let hash = one.0.hash;
        Self { genesis: genesis(), blocks: vec![one, block2(two, hash)] }
    }
}

impl Default for BlockChainTestData {
    fn default() -> Self {
        let one = block1(1);
        let hash = one.0.hash;
        Self { genesis: genesis(), blocks: vec![one, block2(2, hash)] }
    }
}

/// Genesis block
pub fn genesis() -> SealedBlock {
    SealedBlock {
        header: Header { number: 0, difficulty: U256::from(1), ..Default::default() }
            .seal(H256::zero()),
        body: vec![],
        ommers: vec![],
        withdrawals: Some(vec![]),
    }
}

/// Block one that points to genesis
fn block1(number: BlockNumber) -> (SealedBlockWithSenders, PostState) {
    let mut block_rlp = hex!("f9025ff901f7a0c86e8cc0310ae7c531c758678ddbfd16fc51c8cef8cec650b032de9869e8b94fa01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa050554882fbbda2c2fd93fdc466db9946ea262a67f7a76cc169e714f105ab583da00967f09ef1dfed20c0eacfaa94d5cd4002eda3242ac47eae68972d07b106d192a0e3c8b47fbfc94667ef4cceb17e5cc21e3b1eebd442cebb27f07562b33836290db90100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008302000001830f42408238108203e800a00000000000000000000000000000000000000000000000000000000000000000880000000000000000f862f860800a83061a8094095e7baea6a6c7c4c2dfeb977efac326af552d8780801ba072ed817487b84ba367d15d2f039b5fc5f087d0a8882fbdf73e8cb49357e1ce30a0403d800545b8fc544f92ce8124e2255f8c3c6af93f28243a120585d4c4c6a2a3c0").as_slice();
    let mut block = SealedBlock::decode(&mut block_rlp).unwrap();
    block.withdrawals = Some(vec![Withdrawal::default()]);
    let mut header = block.header.clone().unseal();
    header.number = number;
    header.state_root =
        H256(hex!("5d035ccb3e75a9057452ff060b773b213ec1fc353426174068edfc3971a0b6bd"));
    header.parent_hash = H256::zero();
    block.header = header.seal_slow();

    let mut post_state = PostState::default();
    // Transaction changes
    post_state.create_account(
        number,
        H160([0x61; 20]),
        Account { nonce: 1, balance: U256::from(10), bytecode_hash: None },
    );
    post_state.create_account(
        number,
        H160([0x60; 20]),
        Account { nonce: 1, balance: U256::from(10), bytecode_hash: None },
    );
    post_state.change_storage(
        number,
        H160([0x60; 20]),
        BTreeMap::from([(U256::from(5), (U256::ZERO, U256::from(10)))]),
    );

    post_state.add_receipt(
        number,
        Receipt {
            tx_type: TxType::EIP2930,
            success: true,
            cumulative_gas_used: 300,
            logs: vec![Log {
                address: H160([0x60; 20]),
                topics: vec![H256::from_low_u64_be(1), H256::from_low_u64_be(2)],
                data: Bytes::default(),
            }],
        },
    );

    (SealedBlockWithSenders { block, senders: vec![H160([0x30; 20])] }, post_state)
}

/// Block two that points to block 1
fn block2(number: BlockNumber, parent_hash: H256) -> (SealedBlockWithSenders, PostState) {
    let mut block_rlp = hex!("f9025ff901f7a0c86e8cc0310ae7c531c758678ddbfd16fc51c8cef8cec650b032de9869e8b94fa01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa050554882fbbda2c2fd93fdc466db9946ea262a67f7a76cc169e714f105ab583da00967f09ef1dfed20c0eacfaa94d5cd4002eda3242ac47eae68972d07b106d192a0e3c8b47fbfc94667ef4cceb17e5cc21e3b1eebd442cebb27f07562b33836290db90100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008302000001830f42408238108203e800a00000000000000000000000000000000000000000000000000000000000000000880000000000000000f862f860800a83061a8094095e7baea6a6c7c4c2dfeb977efac326af552d8780801ba072ed817487b84ba367d15d2f039b5fc5f087d0a8882fbdf73e8cb49357e1ce30a0403d800545b8fc544f92ce8124e2255f8c3c6af93f28243a120585d4c4c6a2a3c0").as_slice();
    let mut block = SealedBlock::decode(&mut block_rlp).unwrap();
    block.withdrawals = Some(vec![Withdrawal::default()]);
    let mut header = block.header.clone().unseal();
    header.number = number;
    header.state_root =
        H256(hex!("90101a13dd059fa5cca99ed93d1dc23657f63626c5b8f993a2ccbdf7446b64f8"));
    // parent_hash points to block1 hash
    header.parent_hash = parent_hash;
    block.header = header.seal_slow();

    let mut post_state = PostState::default();
    // block changes
    post_state.change_account(
        number,
        H160([0x60; 20]),
        Account { nonce: 1, balance: U256::from(10), bytecode_hash: None },
        Account { nonce: 3, balance: U256::from(20), bytecode_hash: None },
    );
    post_state.change_storage(
        number,
        H160([0x60; 20]),
        BTreeMap::from([(U256::from(5), (U256::from(10), U256::from(15)))]),
    );
    post_state.add_receipt(
        number,
        Receipt {
            tx_type: TxType::EIP1559,
            success: false,
            cumulative_gas_used: 400,
            logs: vec![Log {
                address: H160([0x61; 20]),
                topics: vec![H256::from_low_u64_be(3), H256::from_low_u64_be(4)],
                data: Bytes::default(),
            }],
        },
    );

    (SealedBlockWithSenders { block, senders: vec![H160([0x31; 20])] }, post_state)
}
