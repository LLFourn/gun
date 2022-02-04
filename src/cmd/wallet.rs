use super::*;
use crate::{amount_ext::FromCliStr, betting::BetState, cmd, item};
use bdk::{
    bitcoin::{Address, OutPoint, Script, Txid},
    blockchain::EsploraBlockchain,
    database::Database,
    wallet::{coin_selection::CoinSelectionAlgorithm, tx_builder::TxBuilderContext, AddressIndex},
    KeychainKind, LocalUtxo, SignOptions, TxBuilder,
};
use std::collections::HashMap;
use structopt::StructOpt;

pub fn run_balance(wallet: &GunWallet, sync: bool) -> anyhow::Result<CmdOutput> {
    let (in_bet, unclaimed) = wallet
        .gun_db()
        .list_entities_print_error::<BetState>()
        .filter_map(|(_, bet_state)| match bet_state {
            BetState::Included { bet, .. } => Some((bet.local_value, Amount::ZERO)),
            BetState::Won { bet, .. } => Some((Amount::ZERO, bet.joint_output_value)),
            _ => None,
        })
        .fold((Amount::ZERO, Amount::ZERO), |cur, next| {
            (
                cur.0.checked_add(next.0).unwrap(),
                cur.1.checked_add(next.1).unwrap(),
            )
        });

    let tx_list = wallet
        .bdk_wallet()
        .list_transactions(false)?
        .into_iter()
        .map(|tx_details| (tx_details.txid, tx_details.confirmation_time.is_some()))
        .collect::<Vec<_>>();
    let unspent = wallet.bdk_wallet().list_unspent()?;
    let currently_used = wallet.gun_db().currently_used_utxos(&[])?;

    let (confirmed, unconfirmed, in_use) = unspent.into_iter().fold(
        (Amount::ZERO, Amount::ZERO, Amount::ZERO),
        |(confirmed, unconfirmed, in_use), local_utxo| {
            let is_confirmed = tx_list
                .iter()
                .find_map(|(txid, is_confirmed)| {
                    if *txid == local_utxo.outpoint.txid {
                        Some(*is_confirmed)
                    } else {
                        None
                    }
                })
                .unwrap_or(false);
            let value = Amount::from_sat(local_utxo.txout.value);

            if currently_used
                .iter()
                .any(|outpoint| local_utxo.outpoint == *outpoint)
            {
                (confirmed, unconfirmed, in_use + value)
            } else if is_confirmed {
                (confirmed + value, unconfirmed, in_use)
            } else {
                match local_utxo.keychain {
                    KeychainKind::External => (confirmed, unconfirmed + value, in_use),
                    KeychainKind::Internal => (confirmed + value, unconfirmed, in_use),
                }
            }
        },
    );

    if !sync && (confirmed + unconfirmed + unclaimed + in_bet + in_use == Amount::ZERO) {
        eprintln!("Remember to sync gun with -s or --sync to ensure balances are up to date. i.e. run `gun -s balance` ");
    }

    Ok(item! {
        "confirmed" => Cell::Amount(confirmed),
        "unconfirmed" => Cell::Amount(unconfirmed),
        "unclaimed" => Cell::Amount(unclaimed),
        "available" => Cell::Amount(confirmed + unconfirmed + unclaimed),
        "locked" => Cell::Amount(in_bet),
        "in-use" => Cell::Amount(in_use),
    })
}

#[derive(StructOpt, Debug, Clone)]
pub enum AddressOpt {
    /// A new address even if the last one hasn't been used.
    New,
    /// First one that hasn't been used.
    LastUnused,
    /// List addresses
    List {
        /// Show all addresses (internal & external)
        #[structopt(long, short)]
        all: bool,
        /// Show only internal addresses
        #[structopt(long, short)]
        internal: bool,
        /// Hide addresses with zero balance
        #[structopt(long)]
        hide_zeros: bool,
        /// Show unused addresses
        #[structopt(long)]
        unused: bool,
    },
    /// Show details of an address
    Show { address: Address },
}

fn list_keychain_addresses(
    wallet: &GunWallet,
    keychain_kind: KeychainKind,
    hide_zeros: bool,
    unused: bool,
) -> anyhow::Result<Vec<Vec<Cell>>> {
    let wallet_db = wallet.bdk_wallet().database();
    let scripts = wallet_db.iter_script_pubkeys(Some(keychain_kind))?;
    let txns = wallet_db.iter_txs(true)?;
    let index = wallet_db.get_last_index(keychain_kind)?;
    let map = index_utxos(&*wallet_db)?;
    let rows = match index {
        Some(index) => scripts
            .iter()
            .take(index as usize + 1)
            .filter_map(|script| {
                let address = Address::from_script(script, wallet.bdk_wallet().network()).unwrap();
                let value = map
                    .get(script)
                    .map(|utxos| Amount::from_sat(utxos.iter().map(|utxo| utxo.txout.value).sum()))
                    .unwrap_or(Amount::ZERO);

                let address_used = txns
                    .iter()
                    .flat_map(|tx_details| tx_details.transaction.as_ref())
                    .flat_map(|tx| tx.output.iter())
                    .any(|o| &o.script_pubkey == script);

                if unused && address_used {
                    return None;
                }

                if hide_zeros && (value == Amount::ZERO) {
                    return None;
                }

                let count = map.get(script).map(Vec::len).unwrap_or(0);
                let keychain_name = match keychain_kind {
                    KeychainKind::External => "external",
                    KeychainKind::Internal => "internal",
                }
                .to_string();
                Some(vec![
                    Cell::String(address.to_string()),
                    Cell::Amount(value),
                    Cell::Int(count as u64),
                    Cell::String(keychain_name),
                ])
            })
            // newest should go first
            .rev()
            .collect(),
        None => vec![],
    };
    Ok(rows)
}

pub fn get_address(wallet: &GunWallet, addr_opt: AddressOpt) -> anyhow::Result<CmdOutput> {
    match addr_opt {
        AddressOpt::New => {
            let address = wallet.bdk_wallet().get_address(AddressIndex::New)?;
            Ok(CmdOutput::EmphasisedItem {
                main: ("address", Cell::string(address)),
                other: vec![],
            })
        }
        AddressOpt::LastUnused => {
            let address = wallet.bdk_wallet().get_address(AddressIndex::LastUnused)?;
            Ok(CmdOutput::EmphasisedItem {
                main: ("address", Cell::string(address)),
                other: vec![],
            })
        }
        AddressOpt::List {
            internal,
            hide_zeros,
            all,
            unused,
        } => {
            let mut rows: Vec<Vec<Cell>> = Vec::new();
            let header = vec!["address", "value", "utxos", "keychain"];

            let keychains = match (internal, all) {
                (_, true) => vec![KeychainKind::External, KeychainKind::Internal],
                (true, false) => vec![KeychainKind::Internal],
                _ => vec![KeychainKind::External],
            };

            for keychain in keychains {
                let mut internal_rows =
                    list_keychain_addresses(wallet, keychain, hide_zeros, unused)
                        .expect("fetching addresses from wallet_db");
                rows.append(&mut internal_rows);
            }

            Ok(CmdOutput::table(header, rows))
        }
        AddressOpt::Show { address } => {
            let bdk_db = &*wallet.bdk_wallet().database();
            let script_pubkey = address.script_pubkey();
            let output_descriptor = wallet
                .bdk_wallet()
                .get_descriptor_for_script_pubkey(&address.script_pubkey())?
                .map(|desc| Cell::String(desc.to_string()))
                .unwrap_or(Cell::Empty);
            let keychain = bdk_db
                .get_path_from_script_pubkey(&script_pubkey)?
                .map(|(keychain, _)| {
                    Cell::string(match keychain {
                        KeychainKind::External => "external",
                        KeychainKind::Internal => "internal",
                    })
                })
                .unwrap_or(Cell::Empty);
            let map = index_utxos(bdk_db)?;
            let value = map
                .get(&script_pubkey)
                .map(|utxos| Amount::from_sat(utxos.iter().map(|utxo| utxo.txout.value).sum()))
                .unwrap_or(Amount::ZERO);

            let count = map.get(&script_pubkey).map(Vec::len).unwrap_or(0);

            Ok(item! {
                "value" => Cell::Amount(value),
                "n-utxos" => Cell::Int(count as u64),
                "script-pubkey" => Cell::string(address.script_pubkey().asm()),
                "output-descriptor" => output_descriptor,
                "keychain" => keychain,
            })
        }
    }
}

fn index_utxos(wallet_db: &impl BatchDatabase) -> anyhow::Result<HashMap<Script, Vec<LocalUtxo>>> {
    let mut map: HashMap<Script, Vec<LocalUtxo>> = HashMap::new();
    for local_utxo in wallet_db.iter_utxos()?.iter() {
        map.entry(local_utxo.txout.script_pubkey.clone())
            .and_modify(|v| v.push(local_utxo.clone()))
            .or_insert(vec![local_utxo.clone()]);
    }

    Ok(map)
}

#[derive(StructOpt, Debug, Clone)]
pub struct SendOpt {
    /// The amount to send with denomination e.g. 0.1BTC
    value: ValueChoice,
    /// The address to send the coins to
    to: Address,
    #[structopt(flatten)]
    spend_opt: SpendOpt,
}

#[derive(Clone, Debug, StructOpt)]
pub struct SpendOpt {
    #[structopt(flatten)]
    fee_args: cmd::FeeArgs,
    /// Allow spending utxos that are currently being used in a protocol (like a bet).
    #[structopt(long)]
    spend_in_use: bool,
    /// Don't spend unclaimed coins e.g. coins you won from bets
    #[structopt(long)]
    no_spend_unclaimed: bool,
    /// Also spend bets that are already in the "claiming" state replacing the previous
    /// transaction.
    #[structopt(long)]
    bump_claiming: bool,
    /// Don't prompt for answers just answer yes.
    #[structopt(long, short)]
    yes: bool,
    /// Print the resulting transaction out in hex instead of broadcasting it.
    #[structopt(long)]
    print_tx: bool,
}

impl SpendOpt {
    pub fn spend_coins<D: BatchDatabase, Cs: CoinSelectionAlgorithm<D>, Ctx: TxBuilderContext>(
        self,
        wallet: &GunWallet,
        mut builder: TxBuilder<'_, EsploraBlockchain, D, Cs, Ctx>,
    ) -> anyhow::Result<CmdOutput> {
        let SpendOpt {
            fee_args,
            spend_in_use,
            no_spend_unclaimed,
            bump_claiming,
            yes,
            print_tx,
        } = self;

        builder
            .enable_rbf()
            .ordering(bdk::wallet::tx_builder::TxOrdering::Bip69Lexicographic);

        let in_use = wallet.gun_db().currently_used_utxos(&[])?;

        if !spend_in_use && !in_use.is_empty() {
            eprintln!(
                "note that {} utxos are not availble becuase they are in use",
                in_use.len()
            );
            builder.unspendable(in_use);
        }

        fee_args
            .fee
            .apply_to_builder(wallet.bdk_wallet().client(), &mut builder)?;

        let (mut psbt, claiming_bet_ids) = if !no_spend_unclaimed {
            wallet
                .spend_won_bets(builder, bump_claiming)?
                .expect("Won't be None since builder we pass in is not manually_selected_only")
        } else {
            let (psbt, _) = builder.finish()?;
            (psbt, vec![])
        };

        wallet
            .bdk_wallet()
            .sign(&mut psbt, SignOptions::default())?;

        let finalized = wallet
            .bdk_wallet()
            .finalize_psbt(&mut psbt, SignOptions::default())?;

        assert!(finalized, "transaction must be finalized at this point");

        let (output, txid) = cmd::decide_to_broadcast(
            wallet.bdk_wallet().network(),
            wallet.bdk_wallet().client(),
            psbt,
            yes,
            print_tx,
        )?;

        if let Some(txid) = txid {
            if !print_tx {
                for bet_id in claiming_bet_ids {
                    if let Err(e) = wallet.take_next_action(bet_id, false) {
                        eprintln!(
                            "error updating state of bet {} after broadcasting claim tx {}: {}",
                            bet_id, txid, e
                        );
                    }
                }
            }
        }

        Ok(output)
    }
}
pub fn run_send(wallet: &GunWallet, send_opt: SendOpt) -> anyhow::Result<CmdOutput> {
    let SendOpt {
        to,
        value,
        spend_opt,
    } = send_opt;
    let mut builder = wallet.bdk_wallet().build_tx();

    match value {
        ValueChoice::All => builder.drain_wallet().drain_to(to.script_pubkey()),
        ValueChoice::Amount(amount) => builder.add_recipient(to.script_pubkey(), amount.as_sat()),
    };

    spend_opt.spend_coins(wallet, builder)
}

#[derive(StructOpt, Debug, Clone)]
pub enum TransactionOpt {
    List,
    Show { txid: Txid },
}

pub fn run_transaction_cmd(wallet: &GunWallet, opt: TransactionOpt) -> anyhow::Result<CmdOutput> {
    use TransactionOpt::*;

    match opt {
        List => {
            let mut txns = wallet.bdk_wallet().list_transactions(false)?;

            txns.sort_unstable_by_key(|x| {
                std::cmp::Reverse(
                    x.confirmation_time
                        .as_ref()
                        .map(|x| x.timestamp)
                        .unwrap_or(0),
                )
            });

            let rows: Vec<Vec<Cell>> = txns
                .into_iter()
                .map(|tx| {
                    vec![
                        Cell::String(tx.txid.to_string()),
                        tx.confirmation_time
                            .as_ref()
                            .map(|x| Cell::Int(x.height.into()))
                            .unwrap_or(Cell::Empty),
                        tx.confirmation_time
                            .as_ref()
                            .map(|x| Cell::DateTime(x.timestamp))
                            .unwrap_or(Cell::Empty),
                        Cell::Amount(Amount::from_sat(tx.sent)),
                        Cell::Amount(Amount::from_sat(tx.received)),
                    ]
                })
                .collect();

            Ok(CmdOutput::table(
                vec!["txid", "height", "seen", "sent", "received"],
                rows,
            ))
        }
        Show { txid } => {
            let tx = wallet
                .bdk_wallet()
                .list_transactions(false)?
                .into_iter()
                .find(|tx| tx.txid == txid)
                .ok_or(anyhow!("Transaction {} not found", txid))?;

            Ok(item! {
                "txid" => Cell::String(tx.txid.to_string()),
                "sent" => Cell::Amount(Amount::from_sat(tx.sent)),
                "received" => Cell::Amount(Amount::from_sat(tx.received)),
                "seent-at" => tx.confirmation_time.as_ref()
                            .map(|x| Cell::DateTime(x.timestamp))
                            .unwrap_or(Cell::Empty),
                "confirmed-at" => tx.confirmation_time.as_ref()
                            .map(|x| Cell::Int(x.height.into()))
                            .unwrap_or(Cell::Empty),
                "fee" => tx.fee.map(|x| Cell::Amount(Amount::from_sat(x)))
                    .unwrap_or(Cell::Empty)
            })
        }
    }
}

#[derive(StructOpt, Debug, Clone)]
/// View Unspent Transaction Outputs (UTxOs)
pub enum UtxoOpt {
    /// List UTXOs owned by this wallet
    List,
    /// Show details about a particular UTXO
    Show { outpoint: OutPoint },
}

pub fn run_utxo_cmd(wallet: &GunWallet, opt: UtxoOpt) -> anyhow::Result<CmdOutput> {
    match opt {
        UtxoOpt::List => {
            let in_use_utxos = wallet.gun_db().currently_used_utxos(&[])?;
            let wallet = wallet.bdk_wallet();
            let rows = wallet
                .list_unspent()?
                .into_iter()
                .map(|utxo| {
                    let tx = wallet
                        .query_db(|db| db.get_tx(&utxo.outpoint.txid, false))
                        .unwrap_or(None);
                    vec![
                        Cell::string(utxo.outpoint),
                        Address::from_script(&utxo.txout.script_pubkey, wallet.network())
                            .map(|address| Cell::String(address.to_string()))
                            .unwrap_or(Cell::Empty),
                        Cell::Amount(Amount::from_sat(utxo.txout.value)),
                        Cell::string(match utxo.keychain {
                            KeychainKind::Internal => "internal",
                            KeychainKind::External => "external",
                        }),
                        tx.map(|tx| Cell::String(tx.confirmation_time.is_some().to_string()))
                            .unwrap_or(Cell::Empty),
                        Cell::string(in_use_utxos.contains(&utxo.outpoint)),
                    ]
                })
                .collect();

            // TODO: list won bet utxos
            Ok(CmdOutput::table(
                vec![
                    "outpoint",
                    "address",
                    "value",
                    "keychain",
                    "confirmed",
                    "in-use",
                ],
                rows,
            ))
        }
        UtxoOpt::Show { outpoint } => {
            let utxo = wallet
                .bdk_wallet()
                .query_db(|db| db.get_utxo(&outpoint))?
                .ok_or(anyhow!("UTXO {} not in wallet database", outpoint))?;
            let script_pubkey = utxo.txout.script_pubkey.clone();

            let tx = wallet
                .bdk_wallet()
                .query_db(|db| db.get_tx(&utxo.outpoint.txid, false))?;
            let (tx_seen, tx_height) = tx
                .map(|tx| {
                    (
                        tx.confirmation_time
                            .as_ref()
                            .map(|x| Cell::DateTime(x.timestamp))
                            .unwrap_or(Cell::Empty),
                        tx.confirmation_time
                            .as_ref()
                            .map(|x| Cell::Int(x.height as u64))
                            .unwrap_or(Cell::Empty),
                    )
                })
                .unwrap_or((Cell::Empty, Cell::Empty));

            let output_descriptor = wallet
                .bdk_wallet()
                .get_descriptor_for_script_pubkey(&script_pubkey)?
                .map(|desc| Cell::String(desc.to_string()))
                .unwrap_or(Cell::Empty);
            let in_use = wallet
                .gun_db()
                .currently_used_utxos(&[])?
                .contains(&utxo.outpoint);

            // TODO: show utxos that are associated with won bets
            Ok(item! {
                "outpoint" => Cell::String(utxo.outpoint.to_string()),
                "value" => Cell::Amount(Amount::from_sat(utxo.txout.value)),
                "tx-seen-at" => tx_seen,
                "tx-confirmed-at" => tx_height,
                "address" => Address::from_script(&utxo.txout.script_pubkey, wallet.bdk_wallet().network())
                            .map(|address| Cell::String(address.to_string()))
                            .unwrap_or(Cell::Empty),
                "script-pubkey" => Cell::String(script_pubkey.asm()),
                "output-descriptor" => output_descriptor,
                "keychain" => Cell::String(match utxo.keychain {
                    KeychainKind::External => "external",
                    KeychainKind::Internal => "internal",
                }.into()),
                "in-use" => Cell::string(in_use),
            })
        }
    }
}

#[derive(StructOpt, Debug, Clone)]
pub struct SplitOpt {
    /// The value of each output (best if this divides total)
    #[structopt(parse(try_from_str = FromCliStr::from_cli_str))]
    output_size: Amount,
    /// Number of outputs to create. If omitted it will use the maximum possible.
    n: Option<usize>,
    #[structopt(flatten)]
    spend_opt: SpendOpt,
}

pub fn run_split_cmd(wallet: &GunWallet, opt: SplitOpt) -> anyhow::Result<CmdOutput> {
    let SplitOpt {
        output_size,
        n,
        spend_opt,
    } = opt;
    let bdk_wallet = wallet.bdk_wallet();
    let mut builder = bdk_wallet.build_tx();

    let already_correct = bdk_wallet
        .list_unspent()?
        .into_iter()
        .filter(|utxo| utxo.txout.value == output_size.as_sat());

    builder.unspendable(already_correct.map(|utxo| utxo.outpoint).collect());

    match n {
        Some(n) => {
            for _ in 0..n {
                builder.add_recipient(
                    bdk_wallet
                        .get_change_address(AddressIndex::New)?
                        .address
                        .script_pubkey(),
                    output_size.as_sat(),
                );
            }
        }
        None => {
            builder
                .drain_wallet()
                // add one recipient so we at least get one split utxo of the correct size.
                .add_recipient(
                    bdk_wallet
                        .get_change_address(AddressIndex::New)?
                        .address
                        .script_pubkey(),
                    output_size.as_sat(),
                )
                .split_change(output_size.as_sat(), usize::MAX);
        }
    };

    spend_opt.spend_coins(wallet, builder)
}
