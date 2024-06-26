use super::{
    runner::ProcessState,
    workers::{
        GenesisWorkers,
        Handler,
    },
};
use crate::{
    combined_database::CombinedDatabase,
    database::{
        balances::BalancesInitializer,
        database_description::on_chain::OnChain,
        state::StateInitializer,
        Database,
    },
};
use anyhow::anyhow;
use fuel_core_chain_config::{
    SnapshotReader,
    TableEntry,
};
use fuel_core_storage::{
    tables::{
        Coins,
        ContractsAssets,
        ContractsLatestUtxo,
        ContractsRawCode,
        ContractsState,
        Messages,
        Transactions,
    },
    transactional::StorageTransaction,
    StorageAsMut,
};
use fuel_core_types::{
    self,
    blockchain::primitives::DaBlockHeight,
    entities::{
        coins::coin::Coin,
        Message,
    },
    fuel_types::BlockHeight,
};

pub(crate) async fn import_state(
    db: CombinedDatabase,
    snapshot_reader: SnapshotReader,
) -> anyhow::Result<()> {
    let mut workers = GenesisWorkers::new(db, snapshot_reader);
    if let Err(e) = workers.run_on_chain_imports().await {
        workers.shutdown();
        workers.finished().await;

        return Err(e);
    }

    Ok(())
}

impl ProcessState for Handler<Coins> {
    type TableInSnapshot = Coins;
    type TableBeingWritten = Coins;
    type DbDesc = OnChain;

    fn process(
        &mut self,
        group: Vec<TableEntry<Self::TableInSnapshot>>,
        tx: &mut StorageTransaction<&mut Database>,
    ) -> anyhow::Result<()> {
        group.into_iter().try_for_each(|coin| {
            init_coin(tx, &coin, self.block_height)?;
            Ok(())
        })
    }
}

impl ProcessState for Handler<Messages> {
    type TableInSnapshot = Messages;
    type TableBeingWritten = Messages;
    type DbDesc = OnChain;

    fn process(
        &mut self,
        group: Vec<TableEntry<Self::TableInSnapshot>>,
        tx: &mut StorageTransaction<&mut Database>,
    ) -> anyhow::Result<()> {
        group
            .into_iter()
            .try_for_each(|message| init_da_message(tx, message, self.da_block_height))
    }
}

impl ProcessState for Handler<ContractsRawCode> {
    type TableInSnapshot = ContractsRawCode;
    type TableBeingWritten = ContractsRawCode;
    type DbDesc = OnChain;

    fn process(
        &mut self,
        group: Vec<TableEntry<Self::TableInSnapshot>>,
        tx: &mut StorageTransaction<&mut Database>,
    ) -> anyhow::Result<()> {
        group.into_iter().try_for_each(|contract| {
            init_contract_raw_code(tx, &contract)?;
            Ok::<(), anyhow::Error>(())
        })
    }
}

impl ProcessState for Handler<ContractsLatestUtxo> {
    type TableInSnapshot = ContractsLatestUtxo;
    type TableBeingWritten = ContractsLatestUtxo;
    type DbDesc = OnChain;

    fn process(
        &mut self,
        group: Vec<TableEntry<Self::TableInSnapshot>>,
        tx: &mut StorageTransaction<&mut Database>,
    ) -> anyhow::Result<()> {
        group.into_iter().try_for_each(|contract| {
            init_contract_latest_utxo(tx, &contract, self.block_height)?;
            Ok::<(), anyhow::Error>(())
        })
    }
}

impl ProcessState for Handler<ContractsState> {
    type TableInSnapshot = ContractsState;
    type TableBeingWritten = ContractsState;
    type DbDesc = OnChain;

    fn process(
        &mut self,
        group: Vec<TableEntry<Self::TableInSnapshot>>,
        tx: &mut StorageTransaction<&mut Database>,
    ) -> anyhow::Result<()> {
        tx.update_contract_states(group)?;
        Ok(())
    }
}

impl ProcessState for Handler<ContractsAssets> {
    type TableInSnapshot = ContractsAssets;
    type TableBeingWritten = ContractsAssets;
    type DbDesc = OnChain;

    fn process(
        &mut self,
        group: Vec<TableEntry<Self::TableInSnapshot>>,
        tx: &mut StorageTransaction<&mut Database>,
    ) -> anyhow::Result<()> {
        tx.update_contract_balances(group)?;
        Ok(())
    }
}

impl ProcessState for Handler<Transactions> {
    type TableInSnapshot = Transactions;
    type TableBeingWritten = Transactions;
    type DbDesc = OnChain;

    fn process(
        &mut self,
        group: Vec<TableEntry<Self::TableInSnapshot>>,
        tx: &mut StorageTransaction<&mut Database<Self::DbDesc>>,
    ) -> anyhow::Result<()> {
        for transaction in &group {
            tx.storage::<Transactions>()
                .insert(&transaction.key, &transaction.value)?;
        }
        Ok(())
    }
}

fn init_coin(
    transaction: &mut StorageTransaction<&mut Database>,
    coin: &TableEntry<Coins>,
    height: BlockHeight,
) -> anyhow::Result<()> {
    let utxo_id = coin.key;

    let compressed_coin = Coin {
        utxo_id,
        owner: *coin.value.owner(),
        amount: *coin.value.amount(),
        asset_id: *coin.value.asset_id(),
        tx_pointer: *coin.value.tx_pointer(),
    }
    .compress();

    // ensure coin can't point to blocks in the future
    let coin_height = coin.value.tx_pointer().block_height();
    if coin_height > height {
        return Err(anyhow!(
            "coin tx_pointer height ({coin_height}) cannot be greater than genesis block ({height})"
        ));
    }

    if transaction
        .storage::<Coins>()
        .insert(&utxo_id, &compressed_coin)?
        .is_some()
    {
        return Err(anyhow!("Coin should not exist"));
    }

    Ok(())
}

fn init_contract_latest_utxo(
    transaction: &mut StorageTransaction<&mut Database>,
    entry: &TableEntry<ContractsLatestUtxo>,
    height: BlockHeight,
) -> anyhow::Result<()> {
    let contract_id = entry.key;

    if entry.value.tx_pointer().block_height() > height {
        return Err(anyhow!(
            "contract tx_pointer cannot be greater than genesis block"
        ));
    }

    if transaction
        .storage::<ContractsLatestUtxo>()
        .insert(&contract_id, &entry.value)?
        .is_some()
    {
        return Err(anyhow!("Contract utxo should not exist"));
    }

    Ok(())
}

fn init_contract_raw_code(
    transaction: &mut StorageTransaction<&mut Database>,
    entry: &TableEntry<ContractsRawCode>,
) -> anyhow::Result<()> {
    let contract = entry.value.as_ref();
    let contract_id = entry.key;

    // insert contract code
    if transaction
        .storage::<ContractsRawCode>()
        .insert(&contract_id, contract)?
        .is_some()
    {
        return Err(anyhow!("Contract code should not exist"));
    }

    Ok(())
}

fn init_da_message(
    transaction: &mut StorageTransaction<&mut Database>,
    msg: TableEntry<Messages>,
    da_height: DaBlockHeight,
) -> anyhow::Result<()> {
    let message: Message = msg.value;

    if message.da_height() > da_height {
        return Err(anyhow!(
            "message da_height cannot be greater than genesis da block height"
        ));
    }

    if transaction
        .storage::<Messages>()
        .insert(message.id(), &message)?
        .is_some()
    {
        return Err(anyhow!("Message should not exist"));
    }

    Ok(())
}
